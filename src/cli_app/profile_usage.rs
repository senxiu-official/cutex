//! One-shot, read-only official-profile usage refresh for the Profiles workspace.
use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use chrono::DateTime;
use chrono::Local;
use serde_json::Value;

use super::account_store::ProfileCatalogEntry;
use cutex::profiles::materialize::materialized_profiles_dir;
use cutex::runtime::claude::ClaudeDeployment;

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

pub(super) type ProfileUsageReply = BTreeMap<String, Result<Vec<ProfileUsageMeter>, String>>;
pub(super) type ProfileUsageWorker = Arc<Mutex<std::sync::mpsc::Receiver<ProfileUsageReply>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileUsageMeter {
    pub(crate) label: String,
    pub(crate) remaining_percent: u8,
    pub(crate) balance: Option<String>,
    pub(crate) reset_at: Option<i64>,
}

impl ProfileUsageMeter {
    pub(crate) fn display_value(&self) -> String {
        self.display_value_at(Local::now())
    }

    fn display_value_at(&self, now: DateTime<Local>) -> String {
        if let Some(balance) = self.balance.as_deref() {
            return balance.to_string();
        }
        let filled = ((f64::from(self.remaining_percent) / 5.0).round() as usize).min(20);
        let mut value = format!(
            "[{}{}] {}% left",
            "█".repeat(filled),
            "░".repeat(20 - filled),
            self.remaining_percent
        );
        if let Some(reset_at) = self
            .reset_at
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
            .map(|time| time.with_timezone(&Local))
        {
            value.push_str(&format!(
                " (resets {} {})",
                reset_at.format("%m-%d %H:%M"),
                relative_reset(reset_at, now)
            ));
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileUsageSnapshot {
    Refreshing,
    Unavailable(String),
    Meters(Vec<ProfileUsageMeter>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageKind {
    Codex,
    Claude,
    CustomQuota,
}

#[derive(Debug, Clone)]
struct UsageRequest {
    profile: String,
    profile_id: String,
    kind: UsageKind,
}

pub(super) fn official_refresh_needed(entry: &ProfileCatalogEntry) -> bool {
    entry.source.as_deref() == Some("official")
        && matches!(entry.cli_kind.as_str(), "codex" | "claude")
}

fn custom_quota_refresh_needed(entry: &ProfileCatalogEntry) -> bool {
    !official_refresh_needed(entry) && entry.options.quota.enabled
}

pub(super) fn refresh_needed(entry: &ProfileCatalogEntry) -> bool {
    official_refresh_needed(entry) || custom_quota_refresh_needed(entry)
}

pub(super) fn start(profiles: &[ProfileCatalogEntry]) -> Option<ProfileUsageWorker> {
    let requests = profiles
        .iter()
        .filter(|entry| refresh_needed(entry))
        .map(|entry| UsageRequest {
            profile: entry.name.clone(),
            profile_id: entry.id.clone(),
            kind: if official_refresh_needed(entry) {
                if entry.cli_kind == "claude" {
                    UsageKind::Claude
                } else {
                    UsageKind::Codex
                }
            } else {
                UsageKind::CustomQuota
            },
        })
        .collect::<Vec<_>>();
    if requests.is_empty() {
        return None;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut reply = BTreeMap::new();
        for request in requests {
            let result = match request.kind {
                UsageKind::Codex => read_codex(&request),
                UsageKind::Claude => read_claude(&request),
                UsageKind::CustomQuota => read_custom_quota(&request),
            };
            let result = result.map_err(|error| bounded_error(&error));
            reply.insert(request.profile, result);
        }
        let _ = sender.send(reply);
    });
    Some(Arc::new(Mutex::new(receiver)))
}

fn read_custom_quota(request: &UsageRequest) -> anyhow::Result<Vec<ProfileUsageMeter>> {
    let display = cutex::profiles::quota::read_named(&request.profile)?
        .context("Custom quota is disabled or unavailable")?;
    custom_quota_meter(display).map(|meter| vec![meter])
}

fn custom_quota_meter(
    display: cutex::profiles::quota::Display,
) -> anyhow::Result<ProfileUsageMeter> {
    if display.remaining_percent.is_none() && display.balance.is_none() {
        anyhow::bail!("custom quota has no cached value");
    }
    Ok(ProfileUsageMeter {
        label: if display.stale {
            format!("{} · stale", display.label)
        } else {
            display.label
        },
        remaining_percent: display
            .remaining_percent
            .map(|value| value.round().clamp(0.0, u8::MAX as f64) as u8)
            .unwrap_or_default(),
        balance: display.balance.clone(),
        reset_at: display.reset_at,
    })
}

fn read_codex(request: &UsageRequest) -> anyhow::Result<Vec<ProfileUsageMeter>> {
    let auth_path = materialized_profiles_dir()?
        .join(&request.profile_id)
        .join("auth.json");
    let auth: Value = serde_json::from_slice(&fs::read(&auth_path)?)
        .context("Codex OAuth credential unavailable or malformed")?;
    let access_token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .context("Codex OAuth access token missing")?;
    let mut headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("Accept", "application/json".to_string()),
        ("User-Agent", "cutex".to_string()),
    ];
    if let Some(account_id) = auth
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|account_id| !account_id.is_empty())
    {
        headers.push(("ChatGPT-Account-Id", account_id.to_string()));
    }
    let payload = get_json(CODEX_USAGE_URL, &headers)?;
    format_codex_usage(&payload)
}

/// Subscription windows of the profile's Claude account; API accounts have
/// none (their usage is in `cutex human usage`).
fn read_claude(request: &UsageRequest) -> anyhow::Result<Vec<ProfileUsageMeter>> {
    use cutex::runtime::claude_accounts;
    let default_home = ClaudeDeployment::selected()?.claude_home;
    let (account, home) = claude_accounts::home_for_profile(Some(&request.profile_id), &default_home)?;
    if claude_accounts::load_cached()
        .find(&account)
        .is_some_and(|entry| entry.kind == claude_accounts::ClaudeAccountKind::Api)
    {
        return Ok(Vec::new());
    }
    let payload = cutex::profiles::claude_usage::fetch(&home, &account)
        .with_context(|| format!("Claude account {account}"))?;
    format_claude_usage(&payload)
}

fn get_json(url: &str, headers: &[(&str, String)]) -> anyhow::Result<Value> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .max_redirects(0)
        .build()
        .new_agent();
    let mut request = agent.get(url);
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let mut response = request.call().map_err(|error| match error {
        ureq::Error::StatusCode(status) => {
            anyhow::anyhow!("official usage endpoint returned HTTP {status}")
        }
        _ => anyhow::anyhow!("official usage endpoint unavailable"),
    })?;
    anyhow::ensure!(
        response.status().is_success(),
        "official usage endpoint returned HTTP {}",
        response.status()
    );
    response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_json()
        .map_err(|_| anyhow::anyhow!("invalid or oversized official usage JSON"))
}

fn bounded_error(error: &anyhow::Error) -> String {
    format!("read failed: {error:#}")
}

pub(super) fn format_codex_usage(payload: &Value) -> anyhow::Result<Vec<ProfileUsageMeter>> {
    let mut meters = format_codex_snapshot("Codex", payload.get("rate_limit"));
    if let Some(additional) = payload
        .get("additional_rate_limits")
        .and_then(Value::as_array)
    {
        for snapshot in additional {
            let label = snapshot
                .get("metered_feature")
                .or_else(|| snapshot.get("limit_name"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("additional");
            meters.extend(format_codex_snapshot(label, snapshot.get("rate_limit")));
        }
    }
    anyhow::ensure!(!meters.is_empty(), "response omitted all usage meters");
    Ok(meters)
}

fn format_codex_snapshot(feature: &str, snapshot: Option<&Value>) -> Vec<ProfileUsageMeter> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    let mut meters = Vec::new();
    for (window_kind, window) in [
        ("primary_window", snapshot.get("primary_window")),
        ("secondary_window", snapshot.get("secondary_window")),
    ] {
        let Some(window) = window else {
            continue;
        };
        if let Some(meter) = format_codex_window(feature, window_kind, window) {
            meters.push(meter);
        }
    }
    meters
}

fn format_codex_window(
    feature: &str,
    window_kind: &str,
    window: &Value,
) -> Option<ProfileUsageMeter> {
    if window.is_null() {
        return None;
    }
    let used_percent = window
        .get("used_percent")
        .and_then(Value::as_f64)
        .filter(|value| (0.0..=100.0).contains(value))?;
    let remaining_percent = remaining_percent(used_percent)?;
    let duration = window
        .get("limit_window_seconds")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .map(|value| window_label(value / 60));
    let reset_at = window
        .get("reset_at")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0);
    Some(ProfileUsageMeter {
        label: format!(
            "{feature} {}",
            duration.unwrap_or_else(|| window_kind.to_string())
        ),
        remaining_percent,
        balance: None,
        reset_at,
    })
}

pub(super) fn format_claude_usage(payload: &Value) -> anyhow::Result<Vec<ProfileUsageMeter>> {
    Ok(cutex::profiles::claude_usage::windows(payload)?
        .into_iter()
        .filter_map(|window| {
            Some(ProfileUsageMeter {
                label: window.label,
                remaining_percent: remaining_percent(window.used_percent)?,
                balance: None,
                reset_at: window.reset_at,
            })
        })
        .collect())
}

fn remaining_percent(used_percent: f64) -> Option<u8> {
    let remaining = 100.0 - used_percent;
    if !(0.0..=100.0).contains(&remaining) {
        return None;
    }
    Some(remaining.round().clamp(0.0, u8::MAX as f64) as u8)
}

fn relative_reset(reset_at: DateTime<Local>, now: DateTime<Local>) -> String {
    let seconds = reset_at.timestamp() - now.timestamp();
    if seconds <= 0 {
        return "reset time passed".to_string();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!(
            "in {} {} {} {}",
            days,
            plural(days, "day"),
            hours,
            plural(hours, "hour")
        )
    } else if hours > 0 {
        format!(
            "in {} {} {} {}",
            hours,
            plural(hours, "hour"),
            minutes,
            plural(minutes, "minute")
        )
    } else if minutes > 0 {
        format!("in {} {}", minutes, plural(minutes, "minute"))
    } else {
        "in less than 1 minute".to_string()
    }
}

fn plural(value: i64, singular: &str) -> String {
    if value == 1 {
        singular.to_string()
    } else {
        format!("{singular}s")
    }
}

fn window_label(minutes: i64) -> String {
    match minutes {
        300 => "5h".to_string(),
        10_080 => "7d".to_string(),
        value => format!("{value}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_direct_usage_parses_primary_and_additional_buckets() {
        let payload = json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 26,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 478469,
                    "reset_at": 1790670359
                },
                "secondary_window": null
            },
            "additional_rate_limits": [
                {
                    "metered_feature": "bengalfox",
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 4,
                            "limit_window_seconds": 18000,
                            "reset_at": 1789673036
                        },
                        "secondary_window": {
                            "used_percent": 9,
                            "limit_window_seconds": 604800,
                            "reset_at": 1790259836
                        }
                    }
                }
            ]
        });
        let meters = format_codex_usage(&payload).unwrap();
        assert_eq!(meters.len(), 3);
        assert_eq!(
            meters[0],
            ProfileUsageMeter {
                label: "Codex 7d".to_string(),
                remaining_percent: 74,
                balance: None,
                reset_at: Some(1_790_670_359),
            }
        );
        assert_eq!(meters[1].label, "bengalfox 5h");
        assert_eq!(meters[1].remaining_percent, 96);
        assert_eq!(meters[2].label, "bengalfox 7d");
        assert_eq!(meters[2].remaining_percent, 91);
    }

    #[test]
    fn claude_direct_usage_parses_limits_and_scoped_model() {
        let payload = json!({
            "five_hour": {"utilization": 0.0, "resets_at": null},
            "seven_day": {"utilization": 4.0},
            "limits": [
                {"kind": "session", "group": "session", "percent": 0, "resets_at": null},
                {"kind": "weekly_all", "group": "weekly", "percent": 4,
                 "resets_at": "2026-09-29T17:00:00.077280+00:00"},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 7,
                 "resets_at": "2026-09-29T17:00:00.077501+00:00",
                 "scope": {"model": {"display_name": "Fable"}, "surface": null},
                 "is_active": true}
            ]
        });
        let meters = format_claude_usage(&payload).unwrap();
        assert_eq!(meters.len(), 3);
        assert_eq!(meters[0].label, "5h");
        assert_eq!(meters[0].remaining_percent, 100);
        assert_eq!(meters[1].label, "7d");
        assert_eq!(meters[1].remaining_percent, 96);
        assert_eq!(meters[2].label, "Fable 7d");
        assert_eq!(meters[2].remaining_percent, 93);
    }

    #[test]
    fn claude_direct_usage_tolerates_missing_limits_array() {
        let payload = json!({
            "five_hour": {"utilization": 31.0, "resets_at": "2026-09-24T05:10:00Z"},
            "seven_day": {"utilization": 4.0, "resets_at": "2026-09-30T17:00:00Z"}
        });
        let meters = format_claude_usage(&payload).unwrap();
        assert_eq!(meters.len(), 2);
        assert_eq!(meters[0].label, "5h");
        assert_eq!(meters[0].remaining_percent, 69);
        assert_eq!(meters[1].label, "7d");
        assert_eq!(meters[1].remaining_percent, 96);
    }

    #[test]
    fn usage_meters_render_bars_and_relative_resets() {
        let reset_at = DateTime::parse_from_rfc3339("2026-10-01T03:00:00Z")
            .unwrap()
            .with_timezone(&Local);
        let now = reset_at - chrono::Duration::seconds(2 * 86_400 + 3 * 3_600 + 45 * 60);
        let meter = ProfileUsageMeter {
            label: "7d".to_string(),
            remaining_percent: 93,
            balance: None,
            reset_at: Some(reset_at.timestamp()),
        };
        assert_eq!(
            meter.display_value_at(now),
            format!(
                "[{}{}] 93% left (resets {} in 2 days 3 hours)",
                "█".repeat(19),
                "░",
                reset_at.format("%m-%d %H:%M")
            )
        );

        let now = reset_at - chrono::Duration::seconds(3 * 3_600 + 42 * 60);
        let meter = ProfileUsageMeter {
            reset_at: Some(reset_at.timestamp()),
            ..meter
        };
        assert!(meter
            .display_value_at(now)
            .ends_with("in 3 hours 42 minutes)"));
    }

    #[test]
    fn malformed_official_usage_is_rejected() {
        assert!(format_codex_usage(&json!({})).is_err());
        assert!(format_claude_usage(&json!({})).is_err());
    }

    #[test]
    fn custom_quota_display_reuses_meter_presentation() {
        let meter = custom_quota_meter(cutex::profiles::quota::Display {
            label: "Q".to_string(),
            remaining_percent: Some(73.4),
            balance: None,
            reset_at: Some(1_791_234_000),
            observed_at: Some(1_791_000_000),
            stale: true,
        })
        .unwrap();
        assert_eq!(meter.label, "Q · stale");
        assert_eq!(meter.remaining_percent, 73);
        assert_eq!(meter.reset_at, Some(1_791_234_000));

        let meter = custom_quota_meter(cutex::profiles::quota::Display {
            label: "DS".to_string(),
            remaining_percent: None,
            balance: Some("¥83.22".to_string()),
            reset_at: None,
            observed_at: Some(1_791_000_000),
            stale: false,
        })
        .unwrap();
        assert_eq!(meter.remaining_percent, 0);
        assert_eq!(meter.balance.as_deref(), Some("¥83.22"));
        assert_eq!(meter.display_value(), "¥83.22");

        assert!(custom_quota_meter(cutex::profiles::quota::Display {
            label: "Q".to_string(),
            remaining_percent: None,
            balance: None,
            reset_at: None,
            observed_at: None,
            stale: true,
        })
        .is_err());
    }
}
