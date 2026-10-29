//! Claude subscription "limit resets" (Claude Code's `/limit-reset`,
//! program `cedar_ember`): grants a Claude.ai account holds that clear the
//! current usage limits early.
//!
//! - Read: `GET /api/oauth/usage?cedar_ember=1&skip_spend=1`, block
//!   `cedar_ember {eligible, ineligible_reason, at_limit, grants[{id, label,
//!   resets_total, resets_left, ends_at, paused, usable_now, …}],
//!   next_grant_id, cooldown_until}` (same shapes Claude Code 2.1.301 parses).
//! - Use: `POST /api/organizations/{orgUuid}/reset_rate_limits` `{program:
//!   "cedar_ember", grant_id, request_id}` → `{result: reset | already_used |
//!   not_limited | cooldown | ineligible | unavailable, reason, resets_left}`.
//!   `request_id` makes a retry of the same click safe.
//!
//! The organization UUID is the login's `oauthAccount.organizationUuid` in
//! `<claude_home>/.claude.json` (written by `claude auth login`). Like the
//! usage read, cutex never refreshes the token: a stale one is reported.
use std::path::Path;

use anyhow::Context;
use serde_json::json;
use serde_json::Value;

use super::claude_usage::{self, AccessToken, HttpFailure, STALE_CREDENTIALS_MESSAGE};

const STATUS_URL: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1";
const CLAIM_BASE: &str = "https://api.anthropic.com/api/organizations/";
const PROGRAM: &str = "cedar_ember";
const MAX_GRANTS: usize = 20;

/// One grant, normalized for `/v2/profiles/{p}/reset-credits`.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub id: String,
    pub label: Option<String>,
    pub resets_total: Option<i64>,
    pub resets_left: i64,
    /// Unix seconds.
    pub ends_at: Option<i64>,
    pub paused: bool,
    pub usable_now: bool,
}

/// The `cedar_ember` block (None when the server sent none: not offered).
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub eligible: bool,
    pub ineligible_reason: Option<String>,
    pub at_limit: bool,
    pub grants: Vec<Grant>,
    pub next_grant_id: Option<String>,
    /// Unix seconds.
    pub cooldown_until: Option<i64>,
}

impl Status {
    pub fn available_count(&self) -> i64 {
        self.grants.iter().filter(|grant| !grant.paused).map(|grant| grant.resets_left).sum()
    }
}

/// Claim result, `result` as the server says it.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub result: String,
    pub reason: Option<String>,
    pub resets_left: Option<i64>,
}

fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `request_id` / idempotency key: `[A-Za-z0-9_-]{1,64}`.
pub fn valid_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn timestamp(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(text) => chrono::DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp()),
        Value::Number(number) => number.as_i64().map(|n| if n > 10_000_000_000 { n / 1000 } else { n }),
        _ => None,
    }
}

/// Parses the `cedar_ember` block of a usage payload (defensive: malformed
/// grants are skipped, like Claude Code does).
pub fn parse_status(payload: &Value) -> Option<Status> {
    let block = payload.get("cedar_ember")?.as_object()?;
    let grants: Vec<Grant> = block
        .get("grants")
        .and_then(Value::as_array)
        .map(|grants| {
            grants
                .iter()
                .filter_map(|grant| {
                    let id = grant.get("id")?.as_str()?;
                    if !valid_grant_id(id) {
                        return None;
                    }
                    Some(Grant {
                        id: id.to_string(),
                        label: grant.get("label").and_then(Value::as_str).filter(|l| !l.is_empty()).map(str::to_string),
                        resets_total: grant.get("resets_total").and_then(Value::as_i64),
                        resets_left: grant.get("resets_left")?.as_i64()?.max(0),
                        ends_at: timestamp(grant.get("ends_at")),
                        paused: grant.get("paused").and_then(Value::as_bool).unwrap_or(false),
                        usable_now: grant.get("usable_now").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .take(MAX_GRANTS)
                .collect()
        })
        .unwrap_or_default();
    let next_grant_id = block
        .get("next_grant_id")
        .and_then(Value::as_str)
        .filter(|id| grants.iter().any(|grant| grant.id == *id))
        .map(str::to_string);
    Some(Status {
        eligible: block.get("eligible").and_then(Value::as_bool).unwrap_or(false),
        ineligible_reason: block.get("ineligible_reason").and_then(Value::as_str).map(str::to_string),
        at_limit: block.get("at_limit").and_then(Value::as_bool).unwrap_or(false),
        grants,
        next_grant_id,
        cooldown_until: timestamp(block.get("cooldown_until")),
    })
}

fn token(claude_home: &Path, now_ms: i64) -> anyhow::Result<String> {
    match claude_usage::access_token(claude_home, now_ms)? {
        AccessToken::Valid(token) => Ok(token),
        AccessToken::Stale { .. } => anyhow::bail!(STALE_CREDENTIALS_MESSAGE),
    }
}

fn http_error(what: &str, account: &str, failure: HttpFailure) -> anyhow::Error {
    match failure {
        HttpFailure::Status(status @ (401 | 403)) => anyhow::anyhow!(
            "{what} returned HTTP {status}: the Claude login was rejected; log in again: cutex human claude account login {account}"
        ),
        HttpFailure::Status(429) => anyhow::anyhow!("{what} returned HTTP 429: too many requests, try again later"),
        HttpFailure::Status(status) => anyhow::anyhow!("{what} returned HTTP {status}"),
        HttpFailure::Unavailable => anyhow::anyhow!("{what} unavailable"),
        HttpFailure::Proxy(message) => anyhow::anyhow!("{what} unavailable: {message}"),
        HttpFailure::InvalidBody => anyhow::anyhow!("invalid or oversized {what} JSON"),
    }
}

/// Reads the reset status of `claude_home`; `Ok(None)` = not offered.
pub fn read_status(claude_home: &Path, now_ms: i64, account: &str) -> anyhow::Result<Option<Status>> {
    read_status_with(claude_home, now_ms, account, |token| claude_usage::http_get_url(STATUS_URL, token))
}

fn read_status_with(
    claude_home: &Path,
    now_ms: i64,
    account: &str,
    get: impl FnOnce(&str) -> Result<Value, HttpFailure>,
) -> anyhow::Result<Option<Status>> {
    let token = token(claude_home, now_ms)?;
    let payload = get(&token).map_err(|failure| http_error("official usage endpoint", account, failure))?;
    Ok(parse_status(&payload))
}

/// The login's organization UUID (`<home>/.claude.json`
/// `oauthAccount.organizationUuid`).
pub fn organization_uuid(claude_home: &Path) -> anyhow::Result<String> {
    let path = claude_home.join(".claude.json");
    let raw = std::fs::read(&path)
        .with_context(|| format!("Claude account settings unreadable: {}", path.display()))?;
    let config: Value = serde_json::from_slice(&raw).with_context(|| format!("malformed {}", path.display()))?;
    let uuid = config
        .pointer("/oauthAccount/organizationUuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty() && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .context("the Claude login names no organization; run a Claude session on this account, then try again")?;
    Ok(uuid.to_string())
}

/// Uses one reset of `grant_id` (owner action only).
pub fn claim(claude_home: &Path, now_ms: i64, account: &str, grant_id: &str, request_id: &str) -> anyhow::Result<Claim> {
    claim_with(claude_home, now_ms, account, grant_id, request_id, |url, token, body| {
        let response = claude_usage::http_agent()?
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", "cutex")
            .send_json(body)
            .map_err(claude_usage::http_failure)?;
        claude_usage::read_body(response)
    })
}

fn claim_with(
    claude_home: &Path,
    now_ms: i64,
    account: &str,
    grant_id: &str,
    request_id: &str,
    post: impl FnOnce(&str, &str, &Value) -> Result<Value, HttpFailure>,
) -> anyhow::Result<Claim> {
    anyhow::ensure!(valid_grant_id(grant_id), "malformed reset id");
    anyhow::ensure!(valid_request_id(request_id), "malformed idempotency key");
    let organization = organization_uuid(claude_home)?;
    let token = token(claude_home, now_ms)?;
    let url = format!("{CLAIM_BASE}{organization}/reset_rate_limits");
    let body = json!({ "program": PROGRAM, "grant_id": grant_id, "request_id": request_id });
    let reply = post(&url, &token, &body).map_err(|failure| http_error("limit reset endpoint", account, failure))?;
    let result = reply.get("result").and_then(Value::as_str).unwrap_or("unavailable");
    let known = ["reset", "already_used", "not_limited", "cooldown", "ineligible", "unavailable"];
    Ok(Claim {
        result: if known.contains(&result) { result } else { "unavailable" }.to_string(),
        reason: reply.get("reason").and_then(Value::as_str).map(str::to_string),
        resets_left: reply.get("resets_left").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: i64 = 1_791_435_600_000;

    /// Fixture home only (never a real login).
    fn home(org: Option<&str>) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("claude-resets-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let credentials = json!({"claudeAiOauth": {"accessToken": "fixture-access", "expiresAt": NOW_MS + 3_600_000}});
        std::fs::write(root.join(".credentials.json"), credentials.to_string()).unwrap();
        if let Some(org) = org {
            std::fs::write(root.join(".claude.json"), json!({"oauthAccount": {"organizationUuid": org}}).to_string())
                .unwrap();
        }
        root
    }

    fn payload() -> Value {
        json!({
            "five_hour": {"utilization": 100.0},
            "cedar_ember": {
                "eligible": true, "at_limit": true,
                "grants": [
                    {"id": "grant_a", "label": "Launch gift", "resets_total": 3, "resets_left": 2,
                     "ends_at": "2026-11-30T00:00:00Z", "usable_now": true},
                    {"id": "grant_b", "resets_left": 1, "paused": true},
                    {"id": "bad id!", "resets_left": 5},
                    {"id": "grant_c"}
                ],
                "next_grant_id": "grant_a",
                "cooldown_until": null
            }
        })
    }

    #[test]
    fn status_counts_unpaused_grants_and_skips_malformed_ones() {
        let status = parse_status(&payload()).unwrap();
        assert_eq!(status.grants.len(), 2);
        assert_eq!(status.available_count(), 2);
        assert_eq!(status.next_grant_id.as_deref(), Some("grant_a"));
        assert_eq!(status.grants[0].ends_at, Some(1_795_996_800));
        assert_eq!(status.grants[0].label.as_deref(), Some("Launch gift"));
        assert!(parse_status(&json!({"five_hour": {}})).is_none());
        let unknown_next = parse_status(&json!({"cedar_ember": {"eligible": false, "next_grant_id": "x"}})).unwrap();
        assert_eq!(unknown_next.next_grant_id, None);
        assert_eq!(unknown_next.available_count(), 0);
    }

    #[test]
    fn status_read_sends_the_token_and_maps_rejection() {
        let root = home(None);
        let status = read_status_with(&root, NOW_MS, "default", |token| {
            assert_eq!(token, "fixture-access");
            Ok(payload())
        })
        .unwrap();
        assert_eq!(status.unwrap().available_count(), 2);
        let error = format!("{:#}", read_status_with(&root, NOW_MS, "ca-1", |_| Err(HttpFailure::Status(401))).unwrap_err());
        assert!(error.contains("cutex human claude account login ca-1") && !error.contains("fixture-access"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claim_posts_program_grant_and_request_id_to_the_organization() {
        let root = home(Some("0b5e7a52-1111-4222-8333-944455556666"));
        let claim = claim_with(&root, NOW_MS, "default", "grant_a", "req-1", |url, token, body| {
            assert_eq!(url, "https://api.anthropic.com/api/organizations/0b5e7a52-1111-4222-8333-944455556666/reset_rate_limits");
            assert_eq!(token, "fixture-access");
            assert_eq!(body, &json!({"program": "cedar_ember", "grant_id": "grant_a", "request_id": "req-1"}));
            Ok(json!({"result": "reset", "resets_left": 1}))
        })
        .unwrap();
        assert_eq!(claim, Claim { result: "reset".into(), reason: None, resets_left: Some(1) });
        let odd = claim_with(&root, NOW_MS, "default", "grant_a", "req-2", |_, _, _| Ok(json!({"result": "new_thing"}))).unwrap();
        assert_eq!(odd.result, "unavailable");
        assert!(claim_with(&root, NOW_MS, "default", "bad id!", "req-3", |_, _, _| panic!("no request")).is_err());
        assert!(claim_with(&root, NOW_MS, "default", "grant_a", "", |_, _, _| panic!("no request")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claim_without_an_organization_sends_nothing() {
        let root = home(None);
        let error = format!("{:#}", claim_with(&root, NOW_MS, "default", "grant_a", "req-1", |_, _, _| panic!("no request")).unwrap_err());
        assert!(error.contains(".claude.json"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
