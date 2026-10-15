//! Claude subscription usage (5h / weekly windows) from the OAuth usage
//! endpoint, read with the Claude runtime's own login. Every Claude profile
//! shares that login (one `claude_home` per deployment), so this is one
//! reading for all of them. Shared by the TUI Profiles workspace and the
//! management quota service.
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_WINDOWS: usize = 6;

/// One usage window: `label` is `5h`, `7d`, `Opus 7d`, ….
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeWindow {
    pub label: String,
    pub used_percent: f64,
    pub reset_at: Option<i64>,
}

/// Skip the usage call when the access token expires within this many
/// milliseconds: Claude Code refreshes it only when a session runs, and
/// cutex never refreshes it itself (refresh tokens rotate; racing Claude
/// Code would break the login).
pub const EXPIRY_MARGIN_MS: i64 = 60_000;

/// Result of a usage read.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageRead {
    /// The usage payload.
    Payload(Value),
    /// The stored access token expired (or expires within
    /// [`EXPIRY_MARGIN_MS`]): no request was made. Not an error: Claude Code
    /// refreshes the token the next time a session on this home runs.
    StaleCredentials { expires_at_ms: i64 },
}

/// Text for [`UsageRead::StaleCredentials`] where a plain error is needed.
pub const STALE_CREDENTIALS_MESSAGE: &str =
    "login token expired; it refreshes the next time a Claude session for this account runs";

/// Failure of the usage HTTP call (never carries the token).
#[derive(Debug, Clone, PartialEq)]
pub enum HttpFailure {
    Status(u16),
    Unavailable,
    InvalidBody,
}

/// Raw usage payload, read with `<claude_home>/.credentials.json`. An
/// expired token is an error here ([`STALE_CREDENTIALS_MESSAGE`]); [`read`]
/// tells it apart.
pub fn fetch(claude_home: &Path, account: &str) -> anyhow::Result<Value> {
    match read(claude_home, chrono::Utc::now().timestamp_millis(), account)? {
        UsageRead::Payload(payload) => Ok(payload),
        UsageRead::StaleCredentials { .. } => anyhow::bail!(STALE_CREDENTIALS_MESSAGE),
    }
}

/// Usage of `claude_home` at `now_ms`; `account` names the login in the
/// re-login hint of a rejected token.
pub fn read(claude_home: &Path, now_ms: i64, account: &str) -> anyhow::Result<UsageRead> {
    read_with(claude_home, now_ms, account, http_get)
}

fn read_with(
    claude_home: &Path,
    now_ms: i64,
    account: &str,
    get: impl FnOnce(&str) -> Result<Value, HttpFailure>,
) -> anyhow::Result<UsageRead> {
    let path = claude_home.join(".credentials.json");
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("not logged in: no Claude credentials in {} (missing .credentials.json)", claude_home.display());
        }
        Err(error) => return Err(error).with_context(|| format!("cannot read Claude credentials {}", path.display())),
    };
    let credentials: Value = serde_json::from_slice(&raw)
        .with_context(|| format!("Claude OAuth credential unavailable or malformed: {}", path.display()))?;
    let access_token = credentials
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .context("Claude OAuth access token missing")?;
    if let Some(expires_at_ms) = credentials.pointer("/claudeAiOauth/expiresAt").and_then(Value::as_i64) {
        if expires_at_ms.saturating_sub(now_ms) < EXPIRY_MARGIN_MS {
            return Ok(UsageRead::StaleCredentials { expires_at_ms });
        }
    }
    match get(access_token) {
        Ok(payload) => Ok(UsageRead::Payload(payload)),
        Err(HttpFailure::Status(status @ (401 | 403))) => anyhow::bail!(
            "official usage endpoint returned HTTP {status}: the Claude login was rejected; \
             log in again: cutex human claude account login {account}"
        ),
        Err(HttpFailure::Status(status)) => anyhow::bail!("official usage endpoint returned HTTP {status}"),
        Err(HttpFailure::Unavailable) => anyhow::bail!("official usage endpoint unavailable"),
        Err(HttpFailure::InvalidBody) => anyhow::bail!("invalid or oversized official usage JSON"),
    }
}

fn http_get(access_token: &str) -> Result<Value, HttpFailure> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .build()
        .new_agent();
    let mut response = agent
        .get(CLAUDE_USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "cutex")
        .call()
        .map_err(|error| match error {
            ureq::Error::StatusCode(status) => HttpFailure::Status(status),
            _ => HttpFailure::Unavailable,
        })?;
    if !response.status().is_success() {
        return Err(HttpFailure::Status(response.status().as_u16()));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_json()
        .map_err(|_| HttpFailure::InvalidBody)
}

/// Windows of a usage payload: the `limits` rows, else the top-level
/// `five_hour` / `seven_day*` objects. Errors when none is usable.
pub fn windows(payload: &Value) -> anyhow::Result<Vec<ClaudeWindow>> {
    let mut windows: Vec<ClaudeWindow> = payload
        .get("limits")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().take(MAX_WINDOWS).filter_map(limit_row).collect())
        .unwrap_or_default();
    if windows.is_empty() {
        windows = top_level_rows(payload);
    }
    anyhow::ensure!(!windows.is_empty(), "response omitted all usage meters");
    Ok(windows)
}

fn used(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| (0.0..=100.0).contains(value))
}

fn limit_row(row: &Value) -> Option<ClaudeWindow> {
    let kind = row.get("kind").and_then(Value::as_str)?;
    let used_percent = used(row.get("percent"))?;
    let mut label = match kind {
        "session" => "5h".to_string(),
        "weekly_all" => "7d".to_string(),
        "weekly_scoped" => "scoped 7d".to_string(),
        other => other.replace('_', " "),
    };
    if let Some(model) = row
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
    {
        label = format!("{model} 7d");
    } else if let Some(surface) = row
        .pointer("/scope/surface")
        .and_then(Value::as_str)
        .filter(|surface| !surface.is_empty())
    {
        label = format!("{surface} 7d");
    } else if label == "scoped 7d" {
        label = "Scoped 7d".to_string();
    }
    Some(ClaudeWindow { label, used_percent, reset_at: row.get("resets_at").and_then(reset_timestamp) })
}

fn top_level_rows(payload: &Value) -> Vec<ClaudeWindow> {
    let Some(object) = payload.as_object() else {
        return Vec::new();
    };
    ["five_hour", "seven_day", "seven_day_oauth_apps", "seven_day_opus", "seven_day_sonnet"]
        .into_iter()
        .filter_map(|key| {
            let meter = object.get(key)?;
            let used_percent = used(meter.get("utilization"))?;
            let label = match key {
                "five_hour" => "5h",
                "seven_day" => "7d",
                "seven_day_opus" => "Opus 7d",
                "seven_day_sonnet" => "Sonnet 7d",
                _ => "OAuth apps 7d",
            };
            Some(ClaudeWindow {
                label: label.to_string(),
                used_percent,
                reset_at: meter.get("resets_at").and_then(reset_timestamp),
            })
        })
        .take(MAX_WINDOWS)
        .collect()
}

fn reset_timestamp(value: &Value) -> Option<i64> {
    match value {
        Value::String(raw) => chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|time| time.timestamp()),
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64))
            .filter(|seconds| *seconds > 0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_credentials_explain_login_and_other_io_errors_keep_the_filename() {
        let root = std::env::temp_dir().join(format!("claude-usage-{}", uuid::Uuid::new_v4()));
        let error = format!("{:#}", fetch(&root, "default").unwrap_err());
        assert!(error.contains("not logged in") && error.contains(".credentials.json"), "{error}");
        assert!(error.contains(&root.display().to_string()) && !error.contains("os error"), "{error}");
        std::fs::create_dir_all(root.join(".credentials.json")).unwrap();
        let error = format!("{:#}", fetch(&root, "default").unwrap_err());
        assert!(error.contains("cannot read Claude credentials") && error.contains(".credentials.json"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    fn home_with(credentials: Value) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("claude-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".credentials.json"), serde_json::to_vec(&credentials).unwrap()).unwrap();
        root
    }

    /// Fixture credentials only (never a real login).
    fn fixture(expires_at: Option<i64>) -> Value {
        let mut oauth = json!({"accessToken": "fixture-access", "refreshToken": "fixture-refresh"});
        if let Some(expires_at) = expires_at {
            oauth["expiresAt"] = json!(expires_at);
        }
        json!({"claudeAiOauth": oauth})
    }

    const NOW_MS: i64 = 1_791_435_600_000;

    #[test]
    fn expired_token_skips_the_request() {
        let root = home_with(fixture(Some(NOW_MS - 3_600_000)));
        let read = read_with(&root, NOW_MS, "ca-work1", |_| panic!("no request with an expired token")).unwrap();
        assert_eq!(read, UsageRead::StaleCredentials { expires_at_ms: NOW_MS - 3_600_000 });
        let error = format!("{:#}", fetch(&root, "default").unwrap_err());
        assert_eq!(error, STALE_CREDENTIALS_MESSAGE);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn near_expiry_token_skips_the_request() {
        let root = home_with(fixture(Some(NOW_MS + EXPIRY_MARGIN_MS - 1)));
        let read = read_with(&root, NOW_MS, "ca-work1", |_| panic!("no request near expiry")).unwrap();
        assert!(matches!(read, UsageRead::StaleCredentials { .. }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_or_undated_token_still_reads() {
        for expires_at in [Some(NOW_MS + 3_600_000), None] {
            let root = home_with(fixture(expires_at));
            let mut sent = None;
            let read = read_with(&root, NOW_MS, "ca-work1", |token| {
                sent = Some(token.to_string());
                Ok(json!({"five_hour": {"utilization": 3.0}}))
            })
            .unwrap();
            assert_eq!(sent.as_deref(), Some("fixture-access"));
            assert!(matches!(read, UsageRead::Payload(_)));
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rejected_valid_token_is_an_error_with_a_login_hint() {
        let root = home_with(fixture(Some(NOW_MS + 3_600_000)));
        let error = format!("{:#}", read_with(&root, NOW_MS, "ca-work1", |_| Err(HttpFailure::Status(401))).unwrap_err());
        assert!(error.contains("HTTP 401") && error.contains("cutex human claude account login ca-work1"), "{error}");
        assert!(!error.contains("fixture-access") && !error.contains("fixture-refresh"), "{error}");
        let error = format!("{:#}", read_with(&root, NOW_MS, "ca-work1", |_| Err(HttpFailure::Status(429))).unwrap_err());
        assert_eq!(error, "official usage endpoint returned HTTP 429");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn limits_rows_win_over_top_level_objects() {
        let payload = json!({
            "limits": [
                {"kind": "session", "percent": 12.0, "resets_at": "2026-10-08T05:00:00Z"},
                {"kind": "weekly_scoped", "percent": 40.0, "scope": {"model": {"display_name": "Opus"}}},
                {"kind": "weekly_all", "percent": 101.0}
            ],
            "five_hour": {"utilization": 99.0}
        });
        let windows = windows(&payload).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].reset_at, Some(1_791_435_600));
        assert_eq!(windows[1].label, "Opus 7d");
    }

    #[test]
    fn top_level_objects_are_the_fallback() {
        let payload = json!({"five_hour": {"utilization": 3.5, "resets_at": 1_791_435_600}, "seven_day_sonnet": {"utilization": 20}});
        let windows = windows(&payload).unwrap();
        assert_eq!(windows.iter().map(|w| w.label.as_str()).collect::<Vec<_>>(), ["5h", "Sonnet 7d"]);
        assert!(super::windows(&json!({})).is_err());
    }
}
