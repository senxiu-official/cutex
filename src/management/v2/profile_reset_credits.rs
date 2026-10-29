//! Root-only usage-limit reset credits of a profile's account
//! (`profile-reset-credits`):
//!
//! - `GET /v2/profiles/{profile}/reset-credits[?refresh=true]` — count,
//!   expiry and detail rows; read on request only (no background polling),
//!   cached [`CACHE_SECS`] per account; `refresh=true` bypasses a cache
//!   older than [`MIN_REFRESH_SECS`].
//! - `POST /v2/profiles/{profile}/reset-credits/consume`
//!   `{idempotencyKey, creditId?}` — uses one credit. Owner action only: the
//!   desktop sends it after an explicit confirmation. Retrying with the same
//!   `idempotencyKey` cannot use a second credit.
//!
//! Providers:
//! - Codex profiles with a ChatGPT login: `account/rateLimits/read` (with
//!   details) and `account/rateLimitResetCredit/consume` on a short-lived
//!   app-server for the profile, the same path as native quota.
//! - Claude profiles on a subscription account: Claude Code's limit resets
//!   ([`crate::profiles::claude_resets`]), one reading per Claude account.
//! - Everything else is `supported: false` with `unsupportedReason`:
//!   `apiKey` (API-key login / Claude API account), `zcodeOnly` (GLM / Z.ai:
//!   the reset API belongs to the ZCode service and needs a ZCode login, not
//!   the API key), `provider` (no reset credits known).

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::TcpStream;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::json;
use serde_json::Value;

use crate::http::server::write_json_response;
use crate::http::server::SimpleHttpRequest;
use crate::profiles::claude_resets;
use crate::profiles::model::CliKind;
use crate::profiles::model::StoredAccount;
use crate::profiles::provider_category::ProviderCategory;
use crate::runtime::claude_accounts::ClaudeAccountKind;

pub const CACHE_SECS: i64 = 60;
pub const MIN_REFRESH_SECS: i64 = 10;
pub const CONSUME_METHOD: &str = "account/rateLimitResetCredit/consume";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCreditsRoute {
    View,
    Consume,
}

/// `(profile, route)` for `/v2/profiles/{profile}/reset-credits[/consume]`.
pub fn parse_path(path: &str) -> Option<(String, ResetCreditsRoute)> {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let rest = path.strip_prefix("/v2/profiles/")?;
    let (encoded, route) = if let Some(encoded) = rest.strip_suffix("/reset-credits/consume") {
        (encoded, ResetCreditsRoute::Consume)
    } else {
        (rest.strip_suffix("/reset-credits")?, ResetCreditsRoute::View)
    };
    if encoded.is_empty() || encoded.contains('/') {
        return None;
    }
    let decoded = super::profile_quota::percent_decode(encoded)?;
    (!decoded.is_empty() && !decoded.contains('/') && !decoded.chars().any(char::is_control))
        .then_some((decoded, route))
}

/// Which reset-credit source a profile has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Codex,
    Claude { account: String },
    Unsupported { provider: Option<&'static str>, reason: &'static str },
}

impl Target {
    fn cache_key(&self, profile: &StoredAccount) -> Option<String> {
        match self {
            Target::Codex => Some(format!("codex:{}", profile.id)),
            Target::Claude { account } => Some(format!("claude:{account}")),
            Target::Unsupported { .. } => None,
        }
    }

    fn provider(&self) -> Option<&'static str> {
        match self {
            Target::Codex => Some("codex"),
            Target::Claude { .. } => Some("claude"),
            Target::Unsupported { provider, .. } => *provider,
        }
    }
}

/// Pure decision from the profile facts (tested).
pub fn target_of(
    cli_kind: &CliKind,
    openai_codex: bool,
    chatgpt_login: bool,
    claude_account: Option<(String, ClaudeAccountKind)>,
    category: ProviderCategory,
) -> Target {
    match cli_kind {
        CliKind::Claude => match claude_account {
            Some((_, ClaudeAccountKind::Api)) => Target::Unsupported { provider: Some("claude"), reason: "apiKey" },
            Some((account, _)) => Target::Claude { account },
            None => Target::Unsupported { provider: Some("claude"), reason: "apiKey" },
        },
        _ if matches!(category, ProviderCategory::Glm | ProviderCategory::Zai) => {
            Target::Unsupported { provider: Some("glm"), reason: "zcodeOnly" }
        }
        CliKind::Codex if openai_codex && chatgpt_login => Target::Codex,
        CliKind::Codex if openai_codex => Target::Unsupported { provider: Some("codex"), reason: "apiKey" },
        _ => Target::Unsupported { provider: None, reason: "provider" },
    }
}

fn target(profile: &StoredAccount) -> Target {
    let claude_account = (profile.cli_kind == CliKind::Claude).then(|| {
        let account = super::profile_quota_claude::profile_account(profile);
        let kind = crate::runtime::claude_accounts::load_cached()
            .find(&account)
            .map(|entry| entry.kind)
            .unwrap_or(ClaudeAccountKind::Subscription);
        (account, kind)
    });
    let openai_codex = super::profile_quota_native::openai_codex_profile(profile);
    target_of(
        &profile.cli_kind,
        openai_codex,
        openai_codex && super::profile_quota_native::chatgpt_logged_in(profile),
        claude_account,
        crate::profiles::settings_api::provider_category_of(profile),
    )
}

/// Network side, replaced by fakes in tests (never a real credit).
pub trait ResetBackend {
    fn codex_read(&self, profile: &StoredAccount) -> Result<Value, String>;
    fn codex_consume(&self, profile: &StoredAccount, params: Value) -> Result<Value, String>;
    fn claude_read(&self, account: &str) -> anyhow::Result<Option<claude_resets::Status>>;
    fn claude_claim(&self, account: &str, grant_id: &str, request_id: &str) -> anyhow::Result<claude_resets::Claim>;
}

pub struct LiveBackend;

impl ResetBackend for LiveBackend {
    fn codex_read(&self, profile: &StoredAccount) -> Result<Value, String> {
        super::profile_quota_native::native_request(
            profile,
            super::profile_quota_native::RATE_LIMITS_READ_METHOD,
            json!({}),
        )
    }

    fn codex_consume(&self, profile: &StoredAccount, params: Value) -> Result<Value, String> {
        super::profile_quota_native::native_request(profile, CONSUME_METHOD, params)
    }

    fn claude_read(&self, account: &str) -> anyhow::Result<Option<claude_resets::Status>> {
        let home = crate::runtime::claude_accounts::home_for(account)?;
        claude_resets::read_status(&home, chrono::Utc::now().timestamp_millis(), account)
    }

    fn claude_claim(&self, account: &str, grant_id: &str, request_id: &str) -> anyhow::Result<claude_resets::Claim> {
        let home = crate::runtime::claude_accounts::home_for(account)?;
        claude_resets::claim(&home, chrono::Utc::now().timestamp_millis(), account, grant_id, request_id)
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// `rateLimitResetCredits` of an `account/rateLimits/read` result →
/// `(availableCount, credits, detailsComplete)`.
pub fn codex_summary(read: &Value) -> (Option<i64>, Vec<Value>, bool) {
    let Some(summary) = read.get("rateLimitResetCredits").filter(|v| v.is_object()) else {
        return (None, Vec::new(), false);
    };
    let count = summary.get("availableCount").and_then(Value::as_i64).map(|n| n.max(0));
    let Some(rows) = summary.get("credits").and_then(Value::as_array) else {
        return (count, Vec::new(), false);
    };
    let credits = rows
        .iter()
        .filter(|row| row.get("status").and_then(Value::as_str).is_none_or(|s| s == "available"))
        .filter_map(|row| {
            Some(json!({
                "id": row.get("id")?.as_str()?,
                "title": row.get("title").filter(|v| v.is_string()),
                "description": row.get("description").filter(|v| v.is_string()),
                "status": row.get("status").and_then(Value::as_str).unwrap_or("available"),
                "grantedAt": row.get("grantedAt").and_then(Value::as_i64),
                "expiresAt": row.get("expiresAt").and_then(Value::as_i64),
                "resetsLeft": 1,
            }))
        })
        .collect::<Vec<_>>();
    let complete = count.is_none_or(|count| credits.len() as i64 >= count);
    (count, credits, complete)
}

pub fn claude_summary(status: Option<&claude_resets::Status>) -> Value {
    let Some(status) = status else {
        return json!({ "availableCount": 0, "credits": [], "detailsComplete": true, "offered": false });
    };
    let credits: Vec<Value> = status
        .grants
        .iter()
        .filter(|grant| grant.resets_left > 0)
        .map(|grant| {
            json!({
                "id": grant.id,
                "title": grant.label,
                "description": null,
                "status": if grant.paused { "paused" } else { "available" },
                "grantedAt": null,
                "expiresAt": grant.ends_at,
                "resetsLeft": grant.resets_left,
                "resetsTotal": grant.resets_total,
                "usableNow": grant.usable_now,
            })
        })
        .collect();
    json!({
        "availableCount": status.available_count(),
        "credits": credits,
        "detailsComplete": true,
        "offered": true,
        "eligible": status.eligible,
        "ineligibleReason": status.ineligible_reason,
        "atLimit": status.at_limit,
        "cooldownUntil": status.cooldown_until,
        "nextCreditId": status.next_grant_id,
    })
}

static CACHE: Mutex<Option<HashMap<String, (i64, Value)>>> = Mutex::new(None);
static CONSUMING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn cached(key: &str) -> Option<(i64, Value)> {
    CACHE.lock().ok()?.as_ref()?.get(key).cloned()
}

fn store(key: &str, at: i64, value: &Value) {
    if let Ok(mut guard) = CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key.to_string(), (at, value.clone()));
    }
}

fn forget(key: &str) {
    if let Ok(mut guard) = CACHE.lock() {
        if let Some(map) = guard.as_mut() {
            map.remove(key);
        }
    }
}

/// The body of `GET …/reset-credits` (a read failure is `error`, not a
/// failed request: the owner still sees which account it is).
pub fn view_with(backend: &dyn ResetBackend, profile: &StoredAccount, target: &Target, refresh: bool, now: i64) -> Value {
    let mut body = json!({
        "contractVersion": 2,
        "profile": { "id": profile.id, "name": profile.name },
        "provider": target.provider(),
        "supported": !matches!(target, Target::Unsupported { .. }),
        "unsupportedReason": null,
        "availableCount": null,
        "credits": [],
        "detailsComplete": false,
        "fetchedAt": null,
        "error": null,
    });
    if let Target::Claude { account } = target {
        body["account"] = json!({ "id": account });
    }
    let Some(key) = target.cache_key(profile) else {
        if let Target::Unsupported { reason, .. } = target {
            body["unsupportedReason"] = json!(reason);
        }
        return body;
    };
    if let Some((at, value)) = cached(&key) {
        let age = now.saturating_sub(at);
        if age < CACHE_SECS && (!refresh || age < MIN_REFRESH_SECS) {
            merge(&mut body, &value);
            return body;
        }
    }
    let read: Result<Value, String> = match target {
        Target::Codex => backend.codex_read(profile).map(|read| {
            let (count, credits, complete) = codex_summary(&read);
            json!({ "availableCount": count, "credits": credits, "detailsComplete": complete })
        }),
        Target::Claude { account } => backend
            .claude_read(account)
            .map(|status| claude_summary(status.as_ref()))
            .map_err(|error| format!("{error:#}")),
        Target::Unsupported { .. } => unreachable!("no cache key"),
    };
    let value = match read {
        Ok(mut value) => {
            value["fetchedAt"] = json!(now);
            value
        }
        Err(error) => json!({ "error": error.chars().take(600).collect::<String>(), "fetchedAt": now }),
    };
    store(&key, now, &value);
    merge(&mut body, &value);
    body
}

fn merge(body: &mut Value, value: &Value) {
    if let (Some(body), Some(value)) = (body.as_object_mut(), value.as_object()) {
        for (key, field) in value {
            body.insert(key.clone(), field.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Consuming
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsumeRequest {
    pub idempotency_key: String,
    #[serde(default)]
    pub credit_id: Option<String>,
}

/// Normalized outcome: `reset | nothingToReset | noCredit | alreadyRedeemed |
/// cooldown | ineligible | unavailable`.
pub fn claude_outcome(result: &str) -> &'static str {
    match result {
        "reset" => "reset",
        "already_used" => "alreadyRedeemed",
        "not_limited" => "nothingToReset",
        "cooldown" => "cooldown",
        "ineligible" => "ineligible",
        _ => "unavailable",
    }
}

#[derive(Debug)]
pub enum ConsumeError {
    Unsupported(&'static str),
    InProgress,
    Invalid(String),
    Failed(String),
}

struct Consuming(String);

impl Consuming {
    fn claim(key: &str) -> Option<Self> {
        let mut guard = CONSUMING.lock().ok()?;
        guard.get_or_insert_with(HashSet::new).insert(key.to_string()).then(|| Self(key.to_string()))
    }
}

impl Drop for Consuming {
    fn drop(&mut self) {
        if let Ok(mut guard) = CONSUMING.lock() {
            if let Some(set) = guard.as_mut() {
                set.remove(&self.0);
            }
        }
    }
}

pub fn consume_with(
    backend: &dyn ResetBackend,
    profile: &StoredAccount,
    target: &Target,
    request: &ConsumeRequest,
) -> Result<Value, ConsumeError> {
    if !claude_resets::valid_request_id(&request.idempotency_key) || request.idempotency_key.len() < 8 {
        return Err(ConsumeError::Invalid("idempotencyKey must be 8–64 characters of A–Z a–z 0–9 _ -".into()));
    }
    if let Some(id) = &request.credit_id {
        if id.is_empty() || id.len() > 200 || id.chars().any(char::is_control) {
            return Err(ConsumeError::Invalid("malformed creditId".into()));
        }
    }
    let key = match target.cache_key(profile) {
        Some(key) => key,
        None => {
            let Target::Unsupported { reason, .. } = target else { unreachable!() };
            return Err(ConsumeError::Unsupported(reason));
        }
    };
    let _guard = Consuming::claim(&key).ok_or(ConsumeError::InProgress)?;
    let reply = match target {
        Target::Codex => {
            let mut params = json!({ "idempotencyKey": request.idempotency_key });
            if let Some(id) = &request.credit_id {
                params["creditId"] = json!(id);
            }
            let reply = backend.codex_consume(profile, params).map_err(ConsumeError::Failed)?;
            let outcome = reply.get("outcome").and_then(Value::as_str).unwrap_or("unavailable");
            let outcome = match outcome {
                "reset" | "nothingToReset" | "noCredit" | "alreadyRedeemed" => outcome,
                _ => "unavailable",
            };
            json!({ "outcome": outcome })
        }
        Target::Claude { account } => {
            let grant = match &request.credit_id {
                Some(id) => Some(id.clone()),
                None => {
                    let status = backend.claude_read(account).map_err(|e| ConsumeError::Failed(format!("{e:#}")))?;
                    status.and_then(|status| {
                        status.next_grant_id.clone().or_else(|| {
                            status
                                .grants
                                .iter()
                                .find(|g| !g.paused && g.resets_left > 0)
                                .map(|g| g.id.clone())
                        })
                    })
                }
            };
            match grant {
                None => json!({ "outcome": "noCredit" }),
                Some(grant) => {
                    let claim = backend
                        .claude_claim(account, &grant, &request.idempotency_key)
                        .map_err(|e| ConsumeError::Failed(format!("{e:#}")))?;
                    json!({
                        "outcome": claude_outcome(&claim.result),
                        "reason": claim.reason,
                        "resetsLeft": claim.resets_left,
                    })
                }
            }
        }
        Target::Unsupported { .. } => unreachable!("no cache key"),
    };
    forget(&key);
    let mut body = json!({
        "contractVersion": 2,
        "profile": { "id": profile.id, "name": profile.name },
        "provider": target.provider(),
    });
    merge(&mut body, &reply);
    Ok(body)
}

/// After a reset the windows changed: re-read the quota in the background
/// (publishes `cutex/profile/quota/updated`).
fn refresh_quota_after(profile: &StoredAccount, target: &Target) {
    match target {
        Target::Codex => super::profile_quota_native::spawn_refresh(profile.clone()),
        Target::Claude { account } => {
            let account = account.clone();
            let _ = std::thread::Builder::new().name("cutex-reset-quota".into()).spawn(move || {
                if let Err(error) = super::profile_quota_claude::refresh(&account) {
                    eprintln!("reset credits: quota refresh failed: {error:#}");
                }
            });
        }
        Target::Unsupported { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn refresh_requested(path: &str) -> Result<bool, String> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(false);
    };
    let mut refresh = false;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match (name.as_ref(), value.as_ref()) {
            ("refresh", "true") => refresh = true,
            ("refresh", "false") => refresh = false,
            ("refresh", other) => return Err(format!("refresh must be true or false, got {other}")),
            (other, _) => return Err(format!("unsupported query parameter: {other}")),
        }
    }
    Ok(refresh)
}

pub(super) fn handle(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    profile: &str,
    route: ResetCreditsRoute,
) -> anyhow::Result<()> {
    let error = |stream: &mut TcpStream, status: u16, reason: &str, code: &str, message: &str, retryable: bool| {
        super::server::write_v2_error(stream, status, reason, code, message, retryable, json!({}))
    };
    let account = match crate::profiles::quota::find_profile(profile) {
        Ok(Some(account)) => account,
        Ok(None) => return super::profiles_api::not_found(stream, profile),
        Err(failure) => {
            return super::server::write_v2_error(
                stream,
                503,
                "Service Unavailable",
                "profile_store_unavailable",
                "the profile store could not be read",
                true,
                json!({ "diagnostic": format!("{failure:#}") }),
            )
        }
    };
    let target = target(&account);
    match (request.method.as_str(), route) {
        ("GET", ResetCreditsRoute::View) => {
            let refresh = match refresh_requested(&request.path) {
                Ok(refresh) => refresh,
                Err(message) => return error(stream, 400, "Bad Request", "invalid_request", &message, false),
            };
            let body = view_with(&LiveBackend, &account, &target, refresh, chrono::Utc::now().timestamp());
            write_json_response(stream, 200, "OK", &body)
        }
        ("POST", ResetCreditsRoute::Consume) => {
            let body: ConsumeRequest = match serde_json::from_slice(&request.body) {
                Ok(body) => body,
                Err(failure) => {
                    let message = format!("strict body {{idempotencyKey, creditId?}} required: {failure}");
                    return error(stream, 400, "Bad Request", "invalid_request", &message, false);
                }
            };
            match consume_with(&LiveBackend, &account, &target, &body) {
                Ok(reply) => {
                    if matches!(reply["outcome"].as_str(), Some("reset" | "alreadyRedeemed")) {
                        refresh_quota_after(&account, &target);
                    }
                    write_json_response(stream, 200, "OK", &reply)
                }
                Err(ConsumeError::Unsupported(reason)) => super::server::write_v2_error(
                    stream,
                    409,
                    "Conflict",
                    "reset_credits_unsupported",
                    "this profile's account has no reset credits cutex can use",
                    false,
                    json!({ "unsupportedReason": reason }),
                ),
                Err(ConsumeError::InProgress) => error(
                    stream,
                    409,
                    "Conflict",
                    "reset_credit_consume_in_progress",
                    "a reset for this account is already being used",
                    true,
                ),
                Err(ConsumeError::Invalid(message)) => {
                    error(stream, 400, "Bad Request", "invalid_request", &message, false)
                }
                Err(ConsumeError::Failed(message)) => error(
                    stream,
                    502,
                    "Bad Gateway",
                    "reset_credit_consume_failed",
                    &message.chars().take(600).collect::<String>(),
                    true,
                ),
            }
        }
        _ => error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "use GET for reset-credits, POST for reset-credits/consume",
            false,
        ),
    }
}

#[cfg(test)]
#[path = "profile_reset_credits_tests.rs"]
mod tests;
