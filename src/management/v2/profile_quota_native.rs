//! Cutex-owned native rate-limit quota for ChatGPT-login profiles
//! (`profile-quota-native`).
//!
//! Profiles whose auth file holds a ChatGPT login (Codex, provider `openai`,
//! not an API-key profile) have no HTTP quota endpoint; their 5h / weekly
//! windows and credits come from the native `account/rateLimits/read`.
//! Cutex obtains them without a running session by starting a short-lived
//! app-server for the profile (the same `--auth-file` custody as
//! `profile_login`), and ingests the native `account/rateLimits/updated`
//! notifications of the profile's running runtimes into the same snapshot.
//!
//! The last snapshot per profile is persisted, owner-private, at
//! `<runtime>/profile-quota/native-<profile-id>.json` so it is served
//! immediately after a daemon restart (its `fetchedAt` tells clients how old
//! it is). It holds only the normalized native rate-limit object, never
//! account ids or credentials.
//!
//! Refresh: on demand (`GET /v2/profiles/{p}/quota?refresh=true`, at most
//! once per [`MIN_FORCED_INTERVAL_SECS`]), opportunistically in the
//! background when a GET finds the snapshot missing or due, and by a daemon
//! refresher for recently used profiles every `profileQuotaRefreshMinutes`
//! (global setting, default 5, 0 disables) with exponential backoff after
//! failures. Snapshot changes are published as `cutex/profile/quota/updated`
//! on the host event stream, with `planChanged: true` when the observed
//! account plan (`rateLimits.planType`) changed: the profile's displayed plan
//! label prefers this observation over an older login token
//! (`profiles::plan_label`), so clients re-read `/v2/profiles` then.
//!
//! Precedence: a profile with an enabled quota-config (primary or extra)
//! keeps the HTTP reader exclusively; native gauges are used only without one.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

use crate::profiles::model::CliKind;
use crate::profiles::model::StoredAccount;
use crate::profiles::quota::Gauge;

use super::model::EventEnvelope;
use super::model::EventSource;
use super::model::NativeMessageKind;

pub const PROFILE_QUOTA_UPDATED_EVENT: &str = "cutex/profile/quota/updated";
pub const RATE_LIMITS_READ_METHOD: &str = "account/rateLimits/read";
pub const RATE_LIMITS_UPDATED_METHOD: &str = "account/rateLimits/updated";

/// Gauge ids (stable; clients dedupe live native data against them).
pub const GAUGE_PRIMARY: &str = "codex-primary";
pub const GAUGE_SECONDARY: &str = "codex-secondary";
pub const GAUGE_CREDITS: &str = "codex-credits";

/// Minimum spacing of native reads for one profile, even when forced.
pub const MIN_FORCED_INTERVAL_SECS: i64 = 30;
/// Floor of the background refresh interval.
pub const MIN_BACKGROUND_INTERVAL_SECS: i64 = 60;
/// Ceiling of the failure backoff.
pub const MAX_BACKOFF_SECS: i64 = 3600;
/// "Recently used": launched or seen live within this window.
pub const RECENT_USE_SECS: i64 = 24 * 3600;
/// A snapshot older than this (or 3 intervals) is reported `stale`.
pub const STALE_FLOOR_SECS: i64 = 15 * 60;
const REFRESHER_TICK: Duration = Duration::from_secs(60);
const REFRESHER_FIRST_DELAY: Duration = Duration::from_secs(20);
const SNAPSHOT_VERSION: u32 = 1;
/// Native rate-limit keys kept in the snapshot (all secret-free).
const KEPT_RATE_LIMIT_KEYS: &[&str] = &[
    "limitId",
    "limitName",
    "primary",
    "secondary",
    "credits",
    "planType",
    "rateLimitReachedType",
    "spendControlReached",
    "individualLimit",
];
/// Keys a sparse `account/rateLimits/updated` may omit (or send as null)
/// without meaning "gone"; the previous value is kept.
const STICKY_KEYS: &[&str] = &["primary", "secondary", "credits", "planType"];

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct NativeQuotaSnapshot {
    pub version: u32,
    pub profile_id: String,
    pub profile_name: String,
    /// Unix seconds of the last successful observation (read or notification).
    pub fetched_at: Option<i64>,
    /// `read` (account/rateLimits/read) or `notification` (account/rateLimits/updated).
    pub source: Option<String>,
    /// Normalized native `RateLimitSnapshot` (codex bucket, camelCase).
    pub rate_limits: Option<Value>,
    pub ordinary_usage_allowed: Option<bool>,
    pub reset_credits_available: Option<i64>,
    /// Unix seconds of the last native read attempt.
    pub last_attempt_at: Option<i64>,
    /// Error of the last read attempt, cleared by the next success.
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    /// Unix seconds of the last ingested live notification.
    pub last_notification_at: Option<i64>,
    /// Unix seconds when `rateLimits.planType` was last reported (a read, or
    /// a notification carrying it; sparse notifications keep the plan but
    /// not this time). Snapshots written before it existed: `fetchedAt`.
    /// The profile plan label compares it with the login token's age
    /// (`profiles::plan_label`).
    pub plan_type_observed_at: Option<i64>,
}

/// What a snapshot write changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotChange {
    /// Rendered content (gauges, plan, flags): publish
    /// `cutex/profile/quota/updated`.
    pub content: bool,
    /// The observed account plan (`rateLimits.planType`): the event says
    /// `planChanged` so clients re-read the profile's `planLabel`.
    pub plan: bool,
}

/// A plan value worth showing: non-empty and not the native `unknown`.
fn known_plan(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty() && !plan.eq_ignore_ascii_case("unknown"))
}

impl NativeQuotaSnapshot {
    /// The account plan the native last reported (`rateLimits.planType`).
    pub fn plan_type(&self) -> Option<&str> {
        known_plan(self.rate_limits.as_ref().and_then(|r| r.get("planType")))
    }

    /// The observed account plan and when it was observed.
    pub fn plan_observation(&self) -> Option<(String, i64)> {
        let plan = self.plan_type()?;
        let at = self.plan_type_observed_at.or(self.fetched_at)?;
        Some((plan.to_string(), at))
    }

    fn new(account: &StoredAccount) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            profile_id: account.id.clone(),
            profile_name: account.name.clone(),
            ..Self::default()
        }
    }

    /// Gauges as served, with `stale` derived from age and the last error.
    pub fn gauges(&self, now: i64, interval_secs: i64) -> Vec<Gauge> {
        let (Some(rate_limits), Some(fetched_at)) = (&self.rate_limits, self.fetched_at) else {
            return Vec::new();
        };
        let stale = self.is_stale(now, interval_secs);
        let mut gauges = normalize(rate_limits, fetched_at);
        for gauge in &mut gauges {
            gauge.stale = stale;
        }
        gauges
    }

    pub fn is_stale(&self, now: i64, interval_secs: i64) -> bool {
        let Some(fetched_at) = self.fetched_at else {
            return true;
        };
        let failed_since = self.last_error.is_some()
            && self.last_attempt_at.is_some_and(|attempt| attempt >= fetched_at);
        let max_age = STALE_FLOOR_SECS.max(interval_secs.saturating_mul(3));
        failed_since || now.saturating_sub(fetched_at) > max_age
    }

    /// Secret-free metadata for the quota resource and events.
    pub fn meta(&self, now: i64, interval_secs: i64) -> Value {
        json!({
            "fetchedAt": self.fetched_at,
            "source": self.source,
            "planType": self.rate_limits.as_ref().and_then(|r| r.get("planType")).cloned(),
            "planTypeObservedAt": self.plan_observation().map(|(_, at)| at),
            "rateLimitReachedType": self.rate_limits.as_ref().and_then(|r| r.get("rateLimitReachedType")).cloned(),
            "ordinaryUsageAllowed": self.ordinary_usage_allowed,
            "resetCreditsAvailable": self.reset_credits_available,
            "stale": self.is_stale(now, interval_secs),
            "lastAttemptAt": self.last_attempt_at,
            "lastError": self.last_error,
            "consecutiveFailures": self.consecutive_failures,
            "lastNotificationAt": self.last_notification_at,
        })
    }

    /// Content clients render (observation times excluded) — a change of
    /// this triggers `cutex/profile/quota/updated`.
    fn fingerprint(&self) -> Value {
        let gauges: Vec<Value> = self
            .rate_limits
            .as_ref()
            .map(|r| normalize(r, 0))
            .unwrap_or_default()
            .into_iter()
            .map(|g| serde_json::to_value(g).unwrap_or(Value::Null))
            .collect();
        json!({
            "gauges": gauges,
            "planType": self.rate_limits.as_ref().and_then(|r| r.get("planType")).cloned(),
            "ordinaryUsageAllowed": self.ordinary_usage_allowed,
            "resetCreditsAvailable": self.reset_credits_available,
            "failing": self.last_error.is_some(),
        })
    }
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/// Display caption of a window length: 300 → `5h`, 10080 → `week`.
pub fn window_caption(minutes: i64) -> String {
    match minutes {
        10080 => "week".to_string(),
        1440 => "day".to_string(),
        m if m > 0 && m % 1440 == 0 => format!("{}d", m / 1440),
        m if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|n| n.is_finite())
}

fn window_gauge(id: &str, window: &Value, observed_at: i64) -> Option<Gauge> {
    let window = window.as_object()?;
    let used = finite_number(window.get("usedPercent"))?.clamp(0.0, 100.0);
    let minutes = window.get("windowDurationMins").and_then(Value::as_i64).filter(|m| *m > 0);
    Some(Gauge {
        id: id.to_string(),
        label: "Codex".to_string(),
        kind: "window",
        remaining_percent: Some(100.0 - used),
        balance: None,
        reset_at: window.get("resetsAt").and_then(Value::as_i64).filter(|t| *t > 0),
        window_label: minutes.map(window_caption),
        display_label: None,
        observed_at: Some(observed_at),
        stale: false,
        error: None,
        used_percent: Some(used),
        source: Some("native"),
    })
}

fn credits_gauge(credits: &Value, observed_at: i64) -> Option<Gauge> {
    let credits = credits.as_object()?;
    let unlimited = credits.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
    let balance = if unlimited {
        "unlimited".to_string()
    } else {
        format!("{:.2}", finite_number(credits.get("balance"))?)
    };
    Some(Gauge {
        id: GAUGE_CREDITS.to_string(),
        label: "Credits".to_string(),
        kind: "balance",
        remaining_percent: None,
        balance: Some(balance),
        reset_at: None,
        window_label: None,
        display_label: None,
        observed_at: Some(observed_at),
        stale: false,
        error: None,
        used_percent: None,
        source: Some("native"),
    })
}

/// Native `RateLimitSnapshot` (camelCase) → gauges `codex-primary`,
/// `codex-secondary` (windows with used / remaining percent, reset and
/// caption) and `codex-credits` (balance), each present only when the
/// native reports it.
pub fn normalize(rate_limits: &Value, observed_at: i64) -> Vec<Gauge> {
    let mut gauges = Vec::new();
    if let Some(gauge) = rate_limits
        .get("primary")
        .and_then(|w| window_gauge(GAUGE_PRIMARY, w, observed_at))
    {
        gauges.push(gauge);
    }
    if let Some(gauge) = rate_limits
        .get("secondary")
        .and_then(|w| window_gauge(GAUGE_SECONDARY, w, observed_at))
    {
        gauges.push(gauge);
    }
    if let Some(gauge) = rate_limits.get("credits").and_then(|c| credits_gauge(c, observed_at)) {
        gauges.push(gauge);
    }
    gauges
}

fn kept_rate_limits(rate_limits: &Map<String, Value>) -> Map<String, Value> {
    KEPT_RATE_LIMIT_KEYS
        .iter()
        .filter_map(|key| rate_limits.get(*key).map(|value| ((*key).to_string(), value.clone())))
        .collect()
}

/// A notification / bucket for the codex limit (no id = the legacy codex bucket).
fn is_codex_bucket(rate_limits: &Map<String, Value>) -> bool {
    match rate_limits.get("limitId") {
        None | Some(Value::Null) => true,
        Some(Value::String(id)) => id == "codex",
        Some(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

static STORE_LOCK: Mutex<()> = Mutex::new(());

pub fn store_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("profile-quota"))
}

fn snapshot_path(dir: &Path, profile_id: &str) -> anyhow::Result<PathBuf> {
    let id = uuid::Uuid::parse_str(profile_id)
        .map_err(|_| anyhow::anyhow!("invalid profile id"))?;
    Ok(dir.join(format!("native-{id}.json")))
}

pub fn load_at(dir: &Path, profile_id: &str) -> Option<NativeQuotaSnapshot> {
    let bytes = std::fs::read(snapshot_path(dir, profile_id).ok()?).ok()?;
    serde_json::from_slice::<NativeQuotaSnapshot>(&bytes)
        .ok()
        .filter(|snapshot| snapshot.profile_id == profile_id)
}

pub fn save_at(dir: &Path, snapshot: &NativeQuotaSnapshot) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &snapshot_path(dir, &snapshot.profile_id)?,
        snapshot,
        "native profile quota snapshot",
    )
}

/// Read-modify-write one profile's snapshot. Returns the new snapshot and
/// what changed (rendered content; the observed plan).
fn update_at(
    dir: &Path,
    account: &StoredAccount,
    mutate: impl FnOnce(&mut NativeQuotaSnapshot) -> bool,
) -> anyhow::Result<Option<(NativeQuotaSnapshot, SnapshotChange)>> {
    let _guard = STORE_LOCK.lock().map_err(|_| anyhow::anyhow!("quota store lock poisoned"))?;
    let mut snapshot = load_at(dir, &account.id).unwrap_or_else(|| NativeQuotaSnapshot::new(account));
    let before = snapshot.fingerprint();
    let plan_before = snapshot.plan_type().map(str::to_string);
    if !mutate(&mut snapshot) {
        return Ok(None);
    }
    snapshot.version = SNAPSHOT_VERSION;
    snapshot.profile_name = account.name.clone();
    save_at(dir, &snapshot)?;
    let change = SnapshotChange {
        content: snapshot.fingerprint() != before,
        plan: snapshot.plan_type() != plan_before.as_deref(),
    };
    Ok(Some((snapshot, change)))
}

const MAX_ERROR_CHARS: usize = 600;

fn bounded_error(error: &str) -> String {
    if error.chars().count() <= MAX_ERROR_CHARS {
        return error.to_string();
    }
    let mut bounded: String = error.chars().take(MAX_ERROR_CHARS).collect();
    bounded.push('…');
    bounded
}

/// Apply the outcome of an `account/rateLimits/read`.
pub fn apply_read_at(
    dir: &Path,
    account: &StoredAccount,
    outcome: Result<Value, String>,
    now: i64,
) -> anyhow::Result<(NativeQuotaSnapshot, SnapshotChange)> {
    let applied = update_at(dir, account, |snapshot| {
        snapshot.last_attempt_at = Some(now);
        let parsed = outcome.and_then(|response| {
            let rate_limits = response
                .get("rateLimits")
                .and_then(Value::as_object)
                .ok_or_else(|| "account/rateLimits/read response has no rateLimits".to_string())?;
            Ok((kept_rate_limits(rate_limits), response))
        });
        match parsed {
            Ok((rate_limits, response)) => {
                snapshot.plan_type_observed_at =
                    known_plan(rate_limits.get("planType")).is_some().then_some(now);
                snapshot.rate_limits = Some(Value::Object(rate_limits));
                snapshot.ordinary_usage_allowed =
                    response.get("ordinaryUsageAllowed").and_then(Value::as_bool);
                snapshot.reset_credits_available = response
                    .pointer("/rateLimitResetCredits/availableCount")
                    .and_then(Value::as_i64);
                snapshot.fetched_at = Some(now);
                snapshot.source = Some("read".to_string());
                snapshot.last_error = None;
                snapshot.consecutive_failures = 0;
            }
            Err(error) => {
                snapshot.last_error = Some(bounded_error(&error));
                snapshot.consecutive_failures = snapshot.consecutive_failures.saturating_add(1);
            }
        }
        true
    })?;
    Ok(applied.expect("read outcome always applies"))
}

/// Merge an `account/rateLimits/updated` notification's `rateLimits`.
/// Non-codex buckets and empty updates are ignored (`None`).
pub fn apply_notification_at(
    dir: &Path,
    account: &StoredAccount,
    params: &Value,
    now: i64,
) -> anyhow::Result<Option<(NativeQuotaSnapshot, SnapshotChange)>> {
    let Some(update) = params.get("rateLimits").and_then(Value::as_object) else {
        return Ok(None);
    };
    if !is_codex_bucket(update) {
        return Ok(None);
    }
    let update = kept_rate_limits(update);
    let has_data = ["primary", "secondary", "credits"]
        .iter()
        .any(|key| update.get(*key).is_some_and(|v| !v.is_null()));
    if !has_data {
        return Ok(None);
    }
    let reports_plan = known_plan(update.get("planType")).is_some();
    update_at(dir, account, |snapshot| {
        if reports_plan {
            snapshot.plan_type_observed_at = Some(now);
        } else if snapshot.plan_type_observed_at.is_none() {
            // The sticky plan was observed at the last fetch, not now.
            snapshot.plan_type_observed_at = snapshot.fetched_at;
        }
        let mut merged = snapshot
            .rate_limits
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (key, value) in update {
            if value.is_null() && STICKY_KEYS.contains(&key.as_str()) {
                continue;
            }
            merged.insert(key, value);
        }
        snapshot.rate_limits = Some(Value::Object(merged));
        snapshot.fetched_at = Some(now);
        snapshot.source = Some("notification".to_string());
        snapshot.last_notification_at = Some(now);
        true
    })
}

// ---------------------------------------------------------------------------
// Eligibility and precedence
// ---------------------------------------------------------------------------

/// An enabled quota-config (HTTP reader) gauge wins over native data.
pub fn has_explicit_quota_config(account: &StoredAccount) -> bool {
    account.options.quota.enabled || account.options.quotas.iter().any(|c| c.enabled)
}

/// Codex profile on the OpenAI provider (not an API-key profile).
pub fn openai_codex_profile(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex
        && account.source.as_deref() != Some("api-key")
        && crate::profiles::inspect::account_model_provider(account)
            .is_none_or(|provider| provider.eq_ignore_ascii_case("openai"))
}

/// The profile's auth file holds a ChatGPT login.
pub fn chatgpt_logged_in(account: &StoredAccount) -> bool {
    crate::profiles::materialize::materialized_account_files(account)
        .map(|files| super::profile_login::account_view(&files.auth_path)["authMode"] == "chatgpt")
        .unwrap_or(false)
}

/// Native quota applies: no explicit quota-config, OpenAI Codex profile,
/// ChatGPT login present.
pub fn native_quota_applies(account: &StoredAccount) -> bool {
    !has_explicit_quota_config(account) && openai_codex_profile(account) && chatgpt_logged_in(account)
}

// ---------------------------------------------------------------------------
// Native read
// ---------------------------------------------------------------------------

pub trait RateLimitReader: Send + Sync {
    /// `account/rateLimits/read` result for the profile.
    fn read(&self, account: &StoredAccount) -> Result<Value, String>;
}

/// Short-lived app-server for the profile (shared CODEX_HOME,
/// `--auth-file <profile auth.json>`, file credential store).
pub struct NativeRateLimitReader;

impl RateLimitReader for NativeRateLimitReader {
    fn read(&self, account: &StoredAccount) -> Result<Value, String> {
        native_request(account, RATE_LIMITS_READ_METHOD, json!({ "excludeResetCreditDetails": true }))
    }
}

/// One request to a short-lived app-server for the profile (no turn).
pub fn native_request(account: &StoredAccount, method: &str, params: Value) -> Result<Value, String> {
    use super::profile_login::LoginEndpointFactory;
    let target = super::profile_login::login_target(account).map_err(|e| format!("{e:#}"))?;
    let before = auth_stamp(&target.auth_path);
    let result = super::profile_login::RuntimeLoginFactory
        .spawn(&target)
        .map_err(|error| format!("codex app-server unavailable: {error}"))
        .and_then(|mut endpoint| endpoint.request(method, params).map_err(|error| format!("{method} failed: {error}")));
    // A token refresh rewrites auth.json (inherited ACLs on Windows,
    // which owner launch rejects): restore Cutex's private custody.
    if auth_stamp(&target.auth_path) != before {
        if let Ok(files) = crate::profiles::materialize::materialized_account_files(account) {
            if let Err(error) = crate::profiles::materialize::set_materialized_file_permissions(&files) {
                eprintln!("profile quota: could not re-secure profile auth file: {error:#}");
            }
        }
    }
    result
}

fn auth_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

// ---------------------------------------------------------------------------
// Refresh orchestration
// ---------------------------------------------------------------------------

static IN_FLIGHT: Mutex<Option<HashSet<String>>> = Mutex::new(None);

struct InFlight(String);

impl InFlight {
    fn claim(profile_id: &str) -> Option<Self> {
        let mut guard = IN_FLIGHT.lock().ok()?;
        guard
            .get_or_insert_with(HashSet::new)
            .insert(profile_id.to_string())
            .then(|| Self(profile_id.to_string()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut guard) = IN_FLIGHT.lock() {
            if let Some(set) = guard.as_mut() {
                set.remove(&self.0);
            }
        }
    }
}

pub fn in_flight(profile_id: &str) -> bool {
    IN_FLIGHT
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|set| set.contains(profile_id)))
        .unwrap_or(false)
}

/// Background interval (seconds) from settings; `None` when disabled.
pub fn configured_interval_secs() -> Option<i64> {
    let minutes = crate::config::store::load_codez_config().profile_quota_refresh_minutes;
    (minutes > 0).then(|| (i64::from(minutes) * 60).max(MIN_BACKGROUND_INTERVAL_SECS))
}

/// Interval used for staleness when the background refresh is disabled.
pub fn display_interval_secs() -> i64 {
    configured_interval_secs().unwrap_or(5 * 60)
}

/// Whether a background read is due: neither a read attempt nor a live
/// observation within the interval (doubled per consecutive failure, capped).
pub fn refresh_due(snapshot: Option<&NativeQuotaSnapshot>, now: i64, interval_secs: i64) -> bool {
    let Some(snapshot) = snapshot else {
        return true;
    };
    let shift = snapshot.consecutive_failures.min(6);
    let backoff = if shift == 0 {
        interval_secs
    } else {
        interval_secs.saturating_mul(1 << shift).min(MAX_BACKOFF_SECS).max(interval_secs)
    };
    let attempt_due = snapshot
        .last_attempt_at
        .is_none_or(|attempt| now.saturating_sub(attempt) >= backoff);
    let fresh_live = snapshot.consecutive_failures == 0
        && snapshot
            .fetched_at
            .is_some_and(|fetched| now.saturating_sub(fetched) < interval_secs);
    attempt_due && !fresh_live
}

/// Read the native rate limits now unless one ran within
/// [`MIN_FORCED_INTERVAL_SECS`] or is already running. Returns the current
/// snapshot and what changed.
pub fn refresh_at(
    dir: &Path,
    account: &StoredAccount,
    reader: &dyn RateLimitReader,
    now: i64,
) -> anyhow::Result<(Option<NativeQuotaSnapshot>, SnapshotChange)> {
    let current = load_at(dir, &account.id);
    if current
        .as_ref()
        .and_then(|s| s.last_attempt_at)
        .is_some_and(|attempt| now.saturating_sub(attempt) < MIN_FORCED_INTERVAL_SECS)
    {
        return Ok((current, SnapshotChange::default()));
    }
    let Some(_claim) = InFlight::claim(&account.id) else {
        return Ok((current, SnapshotChange::default()));
    };
    let outcome = reader.read(account);
    let (snapshot, change) = apply_read_at(dir, account, outcome, now)?;
    Ok((Some(snapshot), change))
}

/// Publish + trend-record a changed snapshot (best effort).
fn after_change(snapshot: &NativeQuotaSnapshot, change: SnapshotChange) {
    let interval = display_interval_secs();
    let now = chrono::Utc::now().timestamp();
    let gauges = snapshot.gauges(now, interval);
    if !snapshot.is_stale(now, interval) {
        crate::management::v2::usage::record_quota_gauges(
            &snapshot.profile_id,
            &snapshot.profile_name,
            &gauges,
        );
    }
    publish(snapshot, &gauges, now, interval, change.plan);
}

/// `planChanged`: the observed account plan differs from the previous
/// snapshot, so the profile's `planLabel` (/v2/profiles) may have changed.
pub fn event_params(
    snapshot: &NativeQuotaSnapshot,
    gauges: &[Gauge],
    now: i64,
    interval: i64,
    plan_changed: bool,
) -> Value {
    json!({
        "profile": { "id": snapshot.profile_id, "name": snapshot.profile_name },
        "source": "native",
        "gauges": gauges,
        "nativeQuota": snapshot.meta(now, interval),
        "planChanged": plan_changed,
        "hostId": crate::platform::host::current_host_name(),
    })
}

fn publish(snapshot: &NativeQuotaSnapshot, gauges: &[Gauge], now: i64, interval: i64, plan_changed: bool) {
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    if let Err(error) = super::server::append_cutex_event(
        repository,
        super::services::HOST_EVENT_SESSION_ID,
        super::model::EventCorrelation::default(),
        PROFILE_QUOTA_UPDATED_EVENT,
        event_params(snapshot, gauges, now, interval, plan_changed),
    ) {
        eprintln!("profile quota: could not publish event: {error:#}");
    }
}

/// Synchronous native read for one profile (the `?refresh=true` path).
pub fn refresh(account: &StoredAccount) -> anyhow::Result<Option<NativeQuotaSnapshot>> {
    let (snapshot, change) = refresh_at(
        &store_dir()?,
        account,
        &NativeRateLimitReader,
        chrono::Utc::now().timestamp(),
    )?;
    if change.content {
        if let Some(snapshot) = &snapshot {
            after_change(snapshot, change);
        }
    }
    Ok(snapshot)
}

/// Start a native read on a worker thread (no-op when one is running).
pub fn spawn_refresh(account: StoredAccount) {
    if in_flight(&account.id) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("cutex-profile-quota".to_string())
        .spawn(move || {
            if let Err(error) = refresh(&account) {
                eprintln!("profile quota: refresh failed: {error:#}");
            }
        });
}

/// Quota view of a native-quota profile for `GET /v2/profiles/{p}/quota`:
/// `(gauges, nativeQuota metadata)`. With `force`, reads synchronously;
/// otherwise serves the persisted snapshot and refreshes in the background
/// when it is missing or due.
pub fn quota_view(account: &StoredAccount, force: bool) -> anyhow::Result<(Vec<Gauge>, Value)> {
    let dir = store_dir()?;
    let now = chrono::Utc::now().timestamp();
    let interval = display_interval_secs();
    let mut spawned = false;
    let snapshot = if force {
        refresh(account)?
    } else {
        let snapshot = load_at(&dir, &account.id);
        // Same throttle as a forced read: never more than one per
        // MIN_FORCED_INTERVAL_SECS, whatever the poll rate of clients.
        let recently_attempted = snapshot
            .as_ref()
            .and_then(|s| s.last_attempt_at)
            .is_some_and(|attempt| now.saturating_sub(attempt) < MIN_FORCED_INTERVAL_SECS);
        if !recently_attempted && refresh_due(snapshot.as_ref(), now, interval) && !in_flight(&account.id) {
            spawn_refresh(account.clone());
            spawned = true;
        }
        snapshot
    };
    let snapshot = snapshot.unwrap_or_else(|| NativeQuotaSnapshot::new(account));
    let mut meta = snapshot.meta(now, interval);
    meta["refreshing"] = json!(spawned || in_flight(&account.id));
    Ok((snapshot.gauges(now, interval), meta))
}

// ---------------------------------------------------------------------------
// Live notifications
// ---------------------------------------------------------------------------

/// The `params` of an `account/rateLimits/updated` app-server notification.
pub fn rate_limits_notification(envelope: &EventEnvelope) -> Option<&Value> {
    if envelope.source != EventSource::AppServer {
        return None;
    }
    let native = envelope.native.as_ref()?;
    if native.kind != NativeMessageKind::Notification {
        return None;
    }
    (native.message.get("method").and_then(Value::as_str) == Some(RATE_LIMITS_UPDATED_METHOD))
        .then(|| native.message.get("params"))
        .flatten()
}

/// Runtime event hook: ingest a running runtime's live rate limits into the
/// launched profile's snapshot. Cheap for every other event.
pub fn ingest_runtime_event(envelope: &EventEnvelope, launched_profile: Option<&str>) {
    let Some(params) = rate_limits_notification(envelope) else {
        return;
    };
    let Some(profile) = launched_profile.map(str::trim).filter(|p| !p.is_empty()) else {
        return;
    };
    let result = (|| -> anyhow::Result<()> {
        let Some(account) = crate::profiles::quota::find_profile(profile)? else {
            return Ok(());
        };
        if has_explicit_quota_config(&account) || !openai_codex_profile(&account) {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        if let Some((snapshot, change)) = apply_notification_at(&store_dir()?, &account, params, now)? {
            if change.content {
                after_change(&snapshot, change);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("profile quota: could not ingest live rate limits: {error:#}");
    }
}

// ---------------------------------------------------------------------------
// Background refresher
// ---------------------------------------------------------------------------

/// Recently used: launched (`last_used_at`) or seen live within
/// [`RECENT_USE_SECS`].
pub fn recently_used(account: &StoredAccount, snapshot: Option<&NativeQuotaSnapshot>, now: i64) -> bool {
    let launched = account
        .last_used_at
        .is_some_and(|at| now.saturating_sub(at.timestamp()) < RECENT_USE_SECS);
    let live = snapshot
        .and_then(|s| s.last_notification_at)
        .is_some_and(|at| now.saturating_sub(at) < RECENT_USE_SECS);
    launched || live
}

fn refresher_pass() -> anyhow::Result<()> {
    let Some(interval) = configured_interval_secs() else {
        return Ok(());
    };
    let dir = store_dir()?;
    let store = crate::profiles::settings_api::load_store()?;
    for account in &store.accounts {
        let now = chrono::Utc::now().timestamp();
        if has_explicit_quota_config(account) || !openai_codex_profile(account) {
            continue;
        }
        let snapshot = load_at(&dir, &account.id);
        if !recently_used(account, snapshot.as_ref(), now)
            || !refresh_due(snapshot.as_ref(), now, interval)
            || !chatgpt_logged_in(account)
        {
            continue;
        }
        if let Err(error) = refresh(account) {
            eprintln!("profile quota: background refresh of {} failed: {error:#}", account.name);
        }
    }
    Ok(())
}

static REFRESHER_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Start the daemon's native quota refresher (idempotent).
pub fn start_refresher() {
    if REFRESHER_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("cutex-profile-quota-refresher".to_string())
        .spawn(|| {
            std::thread::sleep(REFRESHER_FIRST_DELAY);
            loop {
                if let Err(error) = refresher_pass() {
                    eprintln!("profile quota: refresher pass failed: {error:#}");
                }
                if let Err(error) = super::profile_quota_claude::refresher_pass() {
                    eprintln!("Claude quota: refresher pass failed: {error:#}");
                }
                std::thread::sleep(REFRESHER_TICK);
            }
        });
    if let Err(error) = spawned {
        eprintln!("profile quota: could not start refresher: {error}");
    }
}

#[cfg(test)]
#[path = "profile_quota_native_tests.rs"]
mod tests;
