//! Cutex-owned quota for Claude profiles (`profile-quota-claude`), per
//! Claude account (`claude-accounts`).
//!
//! A Claude profile runs on its Claude account (`StoredAccount.claude_account`,
//! absent = `default`, the deployment's shared login home). Profiles on the
//! same account share one reading:
//!
//! - subscription accounts (and `default`): the OAuth usage endpoint
//!   ([`crate::profiles::claude_usage`]) read with that account's home,
//!   persisted owner-private at `<runtime>/profile-quota/claude-subscription.json`
//!   (`default`, the pre-accounts file) or `claude-<accountId>.json` (windows
//!   only, never credentials), served as gauges with `source: "claude"`;
//! - API accounts: no windows; `GET …/quota` returns no gauges, `source:
//!   "claudeApi"` and `nativeQuota.apiUsage` = the usage ledger totals of the
//!   account's profiles for today / 7 / 30 days (tokens, requests, estimated
//!   cost), computed on request.
//!
//! Refresh (subscription accounts): on demand (`GET …/quota?refresh=true`,
//! at most once per [`MIN_FORCED_INTERVAL_SECS`] per account), in the
//! background when a GET finds the reading missing or due, and by the daemon
//! refresher every `claudeUsageRefreshMinutes` (global setting, default 5)
//! for each account a Claude profile used in the last day refers to, with
//! exponential backoff per account. A changed reading is published as
//! `cutex/profile/quota/updated` once per Claude profile on that account
//! (payload `claudeAccount {id, label}`).
//!
//! An expired login token (`claudeAiOauth.expiresAt` past or within 60 s)
//! is not a failure: the read makes no request, keeps the last windows and
//! sets `nativeQuota.credentialsExpired` (capability
//! `claude-quota-stale-credentials`). Claude Code refreshes the token when a
//! session on the account runs; cutex never refreshes it.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::profiles::claude_usage::ClaudeWindow;
use crate::profiles::model::CliKind;
use crate::profiles::model::StoredAccount;
use crate::profiles::quota::Gauge;
use crate::runtime::claude_accounts::{self, ClaudeAccountKind, DEFAULT_ACCOUNT_ID};

use super::profile_quota_native::has_explicit_quota_config;
use super::profile_quota_native::MAX_BACKOFF_SECS;
use super::profile_quota_native::MIN_FORCED_INTERVAL_SECS;
use super::profile_quota_native::RECENT_USE_SECS;
use super::profile_quota_native::STALE_FLOOR_SECS;

const DEFAULT_SNAPSHOT_FILE: &str = "claude-subscription.json";
const SNAPSHOT_VERSION: u32 = 1;
const MAX_ERROR_CHARS: usize = 600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredWindow {
    pub label: String,
    pub used_percent: f64,
    pub reset_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeQuotaSnapshot {
    pub version: u32,
    /// Unix seconds of the last successful read.
    pub fetched_at: Option<i64>,
    pub windows: Vec<StoredWindow>,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    /// Unix seconds the stored login token expired at, while the last read
    /// was skipped for it (not a failure: Claude Code refreshes the token
    /// the next time a session on the account runs; cutex never does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials_expired_at: Option<i64>,
}

/// One read of an account.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadOutcome {
    Windows(Vec<ClaudeWindow>),
    /// The login token expired; no request was made.
    StaleCredentials { expires_at_ms: i64 },
    Failed(String),
}

impl From<Result<Vec<ClaudeWindow>, String>> for ReadOutcome {
    fn from(result: Result<Vec<ClaudeWindow>, String>) -> Self {
        match result {
            Ok(windows) => Self::Windows(windows),
            Err(error) => Self::Failed(error),
        }
    }
}

impl ClaudeQuotaSnapshot {
    pub fn is_stale(&self, now: i64, interval_secs: i64) -> bool {
        let Some(fetched_at) = self.fetched_at else {
            return true;
        };
        let failed_since = self.last_error.is_some()
            && self.last_attempt_at.is_some_and(|attempt| attempt >= fetched_at);
        failed_since || now.saturating_sub(fetched_at) > STALE_FLOOR_SECS.max(interval_secs.saturating_mul(3))
    }

    /// `claude-<n>` window gauges (label `Claude`, caption `5h` / `7d` / …).
    pub fn gauges(&self, now: i64, interval_secs: i64) -> Vec<Gauge> {
        let Some(fetched_at) = self.fetched_at else {
            return Vec::new();
        };
        let stale = self.is_stale(now, interval_secs);
        self.windows
            .iter()
            .enumerate()
            .map(|(index, window)| Gauge {
                id: format!("claude-{index}"),
                label: "Claude".to_string(),
                kind: "window",
                remaining_percent: Some(100.0 - window.used_percent),
                balance: None,
                reset_at: window.reset_at,
                window_label: Some(window.label.clone()),
                display_label: None,
                observed_at: Some(fetched_at),
                stale,
                error: None,
                used_percent: Some(window.used_percent),
                source: Some("claude"),
            })
            .collect()
    }

    pub fn meta(&self, now: i64, interval_secs: i64) -> Value {
        json!({
            "fetchedAt": self.fetched_at,
            "source": "read",
            "stale": self.is_stale(now, interval_secs),
            "lastAttemptAt": self.last_attempt_at,
            "lastError": self.last_error,
            "consecutiveFailures": self.consecutive_failures,
            "credentialsExpired": self.credentials_expired_at.is_some(),
            "credentialsExpiredAt": self.credentials_expired_at,
        })
    }

    fn fingerprint(&self) -> Value {
        json!({
            "windows": self.windows,
            "failing": self.last_error.is_some(),
            "credentialsExpired": self.credentials_expired_at.is_some(),
        })
    }
}

/// Claude profiles without an enabled quota-config.
pub fn claude_quota_applies(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Claude && !has_explicit_quota_config(account)
}

/// The Claude account a Claude profile runs on.
pub fn profile_account(profile: &StoredAccount) -> String {
    profile
        .claude_account
        .clone()
        .unwrap_or_else(|| DEFAULT_ACCOUNT_ID.to_string())
}

// ---------------------------------------------------------------------------
// Persistence and reads
// ---------------------------------------------------------------------------

static STORE_LOCK: Mutex<()> = Mutex::new(());
static IN_FLIGHT: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// `claude-subscription.json` for `default` (the file before accounts),
/// `claude-<id>.json` for an added account.
pub fn snapshot_path(dir: &Path, account: &str) -> PathBuf {
    if account == DEFAULT_ACCOUNT_ID || claude_accounts::validate_id(account).is_err() {
        return dir.join(DEFAULT_SNAPSHOT_FILE);
    }
    dir.join(format!("claude-{account}.json"))
}

pub fn load_at(dir: &Path, account: &str) -> Option<ClaudeQuotaSnapshot> {
    serde_json::from_slice(&std::fs::read(snapshot_path(dir, account)).ok()?).ok()
}

/// Apply one read outcome; returns the snapshot and whether its rendered
/// content changed.
pub fn apply_read_at(
    dir: &Path,
    account: &str,
    outcome: impl Into<ReadOutcome>,
    now: i64,
) -> anyhow::Result<(ClaudeQuotaSnapshot, bool)> {
    let _guard = STORE_LOCK.lock().map_err(|_| anyhow::anyhow!("quota store lock poisoned"))?;
    let mut snapshot = load_at(dir, account).unwrap_or_default();
    let before = snapshot.fingerprint();
    snapshot.version = SNAPSHOT_VERSION;
    snapshot.last_attempt_at = Some(now);
    match outcome.into() {
        ReadOutcome::Windows(windows) => {
            snapshot.windows = windows
                .into_iter()
                .map(|w| StoredWindow { label: w.label, used_percent: w.used_percent, reset_at: w.reset_at })
                .collect();
            snapshot.fetched_at = Some(now);
            snapshot.last_error = None;
            snapshot.consecutive_failures = 0;
            snapshot.credentials_expired_at = None;
        }
        // Keep the last windows and fetchedAt; not a failure (no alert, no
        // backoff: the next due read only re-reads the credentials file).
        ReadOutcome::StaleCredentials { expires_at_ms } => {
            snapshot.last_error = None;
            snapshot.consecutive_failures = 0;
            snapshot.credentials_expired_at = Some(expires_at_ms.div_euclid(1000));
        }
        ReadOutcome::Failed(error) => {
            snapshot.credentials_expired_at = None;
            let mut error: String = error.chars().take(MAX_ERROR_CHARS).collect();
            if error.len() >= MAX_ERROR_CHARS {
                error.push('…');
            }
            snapshot.last_error = Some(error);
            snapshot.consecutive_failures = snapshot.consecutive_failures.saturating_add(1);
        }
    }
    std::fs::create_dir_all(dir)?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &snapshot_path(dir, account),
        &snapshot,
        "Claude profile quota snapshot",
    )?;
    let changed = snapshot.fingerprint() != before;
    Ok((snapshot, changed))
}

fn read_windows(account: &str) -> ReadOutcome {
    use crate::profiles::claude_usage::{self, UsageRead};
    let home = match claude_accounts::home_for(account) {
        Ok(home) => home,
        Err(error) => return ReadOutcome::Failed(format!("{error:#}")),
    };
    let read = claude_usage::read(&home, chrono::Utc::now().timestamp_millis(), account).and_then(|read| match read {
        UsageRead::Payload(payload) => claude_usage::windows(&payload).map(ReadOutcome::Windows),
        UsageRead::StaleCredentials { expires_at_ms } => Ok(ReadOutcome::StaleCredentials { expires_at_ms }),
    });
    read.unwrap_or_else(|error| ReadOutcome::Failed(format!("read failed for Claude account {account}: {error:#}")))
}

pub fn interval_secs() -> i64 {
    let minutes = crate::config::store::load_codez_config().claude_usage_refresh_minutes;
    i64::from(minutes.max(1)) * 60
}

pub fn refresh_due(snapshot: Option<&ClaudeQuotaSnapshot>, now: i64, interval_secs: i64) -> bool {
    let Some(snapshot) = snapshot else {
        return true;
    };
    let shift = snapshot.consecutive_failures.min(6);
    let backoff = interval_secs.saturating_mul(1 << shift).min(MAX_BACKOFF_SECS).max(interval_secs);
    snapshot
        .last_attempt_at
        .is_none_or(|attempt| now.saturating_sub(attempt) >= backoff)
}

/// One read in flight per account.
struct Claim(String);
impl Claim {
    fn take(account: &str) -> Option<Self> {
        let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        in_flight.insert(account.to_string()).then(|| Self(account.to_string()))
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
    }
}

fn in_flight(account: &str) -> bool {
    IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).contains(account)
}

/// Read `account` now unless a read ran in the last
/// [`MIN_FORCED_INTERVAL_SECS`] or one is in flight; publish on change.
pub fn refresh(account: &str) -> anyhow::Result<Option<ClaudeQuotaSnapshot>> {
    let dir = super::profile_quota_native::store_dir()?;
    let now = chrono::Utc::now().timestamp();
    let current = load_at(&dir, account);
    if current
        .as_ref()
        .and_then(|s| s.last_attempt_at)
        .is_some_and(|attempt| now.saturating_sub(attempt) < MIN_FORCED_INTERVAL_SECS)
    {
        return Ok(current);
    }
    let Some(_claim) = Claim::take(account) else {
        return Ok(current);
    };
    let (snapshot, changed) = apply_read_at(&dir, account, read_windows(account), now)?;
    if changed {
        publish(account, &snapshot);
    }
    Ok(Some(snapshot))
}

fn spawn_refresh(account: &str) {
    if in_flight(account) {
        return;
    }
    let account = account.to_string();
    let _ = std::thread::Builder::new()
        .name("cutex-claude-quota".to_string())
        .spawn(move || {
            if let Err(error) = refresh(&account) {
                eprintln!("Claude quota: refresh of account {account} failed: {error:#}");
            }
        });
}

/// `{id, label, kind}` of a Claude account for quota payloads.
fn account_ref(account: &str) -> Value {
    let registry = claude_accounts::load_cached();
    let entry = registry.find(account);
    json!({
        "id": account,
        "label": entry.map(|entry| entry.label.clone()),
        "kind": entry.map(|entry| entry.kind.as_str()).unwrap_or("subscription"),
    })
}

fn account_kind(account: &str) -> ClaudeAccountKind {
    claude_accounts::load_cached()
        .find(account)
        .map(|entry| entry.kind)
        .unwrap_or(ClaudeAccountKind::Subscription)
}

/// `GET /v2/profiles/{p}/quota` for a Claude profile: `(gauges, nativeQuota,
/// source)`.
pub fn quota_view(profile: &StoredAccount, force: bool) -> anyhow::Result<(Vec<Gauge>, Value, &'static str)> {
    let account = profile_account(profile);
    if account_kind(&account) == ClaudeAccountKind::Api {
        let store = crate::profiles::settings_api::load_store()?;
        let profile_ids: Vec<String> = claude_accounts::profiles_using(&store, &account)
            .into_iter()
            .map(|profile| profile.id)
            .collect();
        let meta = json!({
            "source": "usageLedger",
            "claudeAccount": account_ref(&account),
            "apiUsage": super::usage_api::profile_usage_windows(&profile_ids)?,
        });
        return Ok((Vec::new(), meta, "claudeApi"));
    }
    let dir = super::profile_quota_native::store_dir()?;
    let now = chrono::Utc::now().timestamp();
    let interval = interval_secs();
    let mut spawned = false;
    let snapshot = if force {
        refresh(&account)?
    } else {
        let snapshot = load_at(&dir, &account);
        if refresh_due(snapshot.as_ref(), now, interval) {
            spawn_refresh(&account);
            spawned = true;
        }
        snapshot
    };
    let snapshot = snapshot.unwrap_or_default();
    let mut meta = snapshot.meta(now, interval);
    meta["refreshing"] = json!(spawned || in_flight(&account));
    meta["claudeAccount"] = account_ref(&account);
    Ok((snapshot.gauges(now, interval), meta, "claude"))
}

fn publish(account: &str, snapshot: &ClaudeQuotaSnapshot) {
    let Ok(store) = crate::profiles::settings_api::load_store() else {
        return;
    };
    let Ok(repository) = super::repository::management_v2_repository() else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    let interval = interval_secs();
    let gauges = snapshot.gauges(now, interval);
    let mut meta = snapshot.meta(now, interval);
    meta["claudeAccount"] = account_ref(account);
    for profile in store
        .accounts
        .iter()
        .filter(|profile| claude_quota_applies(profile) && profile_account(profile) == account)
    {
        if !snapshot.is_stale(now, interval) {
            crate::management::v2::usage::record_quota_gauges(&profile.id, &profile.name, &gauges);
        }
        if let Err(error) = super::server::append_cutex_event(
            repository,
            super::services::HOST_EVENT_SESSION_ID,
            super::model::EventCorrelation::default(),
            super::profile_quota_native::PROFILE_QUOTA_UPDATED_EVENT,
            event_params(profile, &gauges, &meta, account_ref(account)),
        ) {
            eprintln!("Claude quota: could not publish event: {error:#}");
        }
    }
}

/// `cutex/profile/quota/updated` params for one Claude profile.
fn event_params(profile: &StoredAccount, gauges: &[Gauge], meta: &Value, account: Value) -> Value {
    json!({
        "profile": { "id": profile.id, "name": profile.name },
        "source": "claude",
        "gauges": gauges,
        "nativeQuota": meta,
        "claudeAccount": account,
        "hostId": crate::platform::host::current_host_name(),
    })
}

/// Subscription accounts referenced by Claude profiles used in the last
/// day (API accounts have no windows to poll).
pub fn accounts_to_refresh(profiles: &[StoredAccount], registry: &claude_accounts::ClaudeAccountsFile, now: i64) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for profile in profiles {
        if !claude_quota_applies(profile)
            || !profile
                .last_used_at
                .is_some_and(|at| now.saturating_sub(at.timestamp()) < RECENT_USE_SECS)
        {
            continue;
        }
        let account = profile_account(profile);
        let subscription = account == DEFAULT_ACCOUNT_ID
            || registry
                .find(&account)
                .is_some_and(|entry| entry.kind == ClaudeAccountKind::Subscription);
        if subscription && seen.insert(account.clone()) {
            out.push(account);
        }
    }
    out
}

/// One refresher tick: read each due account a Claude profile used in the
/// last day refers to. One account's failure does not stop the others.
pub fn refresher_pass() -> anyhow::Result<()> {
    let store = crate::profiles::settings_api::load_store()?;
    let now = chrono::Utc::now().timestamp();
    let accounts = accounts_to_refresh(&store.accounts, &claude_accounts::load_cached(), now);
    if accounts.is_empty() {
        return Ok(());
    }
    let dir = super::profile_quota_native::store_dir()?;
    let interval = interval_secs();
    for account in accounts {
        if refresh_due(load_at(&dir, &account).as_ref(), now, interval) {
            if let Err(error) = refresh(&account) {
                eprintln!("Claude quota: refresh of account {account} failed: {error:#}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-claude-quota-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn account(cli_kind: &str) -> StoredAccount {
        serde_json::from_value(json!({ "id": uuid::Uuid::new_v4().to_string(), "name": "rowan", "cli_kind": cli_kind })).unwrap()
    }

    fn window(label: &str, used: f64) -> ClaudeWindow {
        ClaudeWindow { label: label.to_string(), used_percent: used, reset_at: Some(1_791_435_600) }
    }

    #[test]
    fn reads_become_gauges_and_failures_mark_them_stale() {
        let dir = temp_dir();
        let (snapshot, changed) =
            apply_read_at(&dir, DEFAULT_ACCOUNT_ID, Ok(vec![window("5h", 12.0), window("7d", 40.0)]), 1000).unwrap();
        assert!(changed);
        let gauges = snapshot.gauges(1000, 300);
        assert_eq!(gauges.len(), 2);
        assert_eq!(gauges[0].id, "claude-0");
        assert_eq!(gauges[0].window_label.as_deref(), Some("5h"));
        assert_eq!(gauges[0].remaining_percent, Some(88.0));
        assert_eq!(gauges[1].source, Some("claude"));
        assert!(!gauges[0].stale);

        let (same, changed) =
            apply_read_at(&dir, DEFAULT_ACCOUNT_ID, Ok(vec![window("5h", 12.0), window("7d", 40.0)]), 1100).unwrap();
        assert!(!changed);
        assert_eq!(same.fetched_at, Some(1100));

        let (failed, changed) = apply_read_at(&dir, DEFAULT_ACCOUNT_ID, Err("read failed: HTTP 429".into()), 1200).unwrap();
        assert!(changed);
        assert_eq!(failed.consecutive_failures, 1);
        assert_eq!(failed.windows.len(), 2);
        assert!(failed.gauges(1200, 300).iter().all(|g| g.stale));
        assert_eq!(load_at(&dir, DEFAULT_ACCOUNT_ID).unwrap(), failed);
        // The default account keeps the pre-accounts file name.
        assert!(dir.join("claude-subscription.json").is_file());
    }

    #[test]
    fn each_account_has_its_own_snapshot_and_backoff() {
        let dir = temp_dir();
        apply_read_at(&dir, DEFAULT_ACCOUNT_ID, Ok(vec![window("5h", 10.0)]), 1000).unwrap();
        let (work, _) = apply_read_at(&dir, "ca-work1", Err("read failed: HTTP 401".into()), 1000).unwrap();
        assert_eq!(snapshot_path(&dir, "ca-work1"), dir.join("claude-ca-work1.json"));
        assert_eq!(work.consecutive_failures, 1);
        assert_eq!(load_at(&dir, DEFAULT_ACCOUNT_ID).unwrap().consecutive_failures, 0);
        assert_eq!(load_at(&dir, DEFAULT_ACCOUNT_ID).unwrap().windows[0].used_percent, 10.0);
        // An id that is not an account id never becomes a path segment.
        assert_eq!(snapshot_path(&dir, "../x"), dir.join("claude-subscription.json"));
    }

    #[test]
    fn quota_events_match_the_method_schema() {
        let dir = temp_dir();
        let (snapshot, _) = apply_read_at(&dir, "ca-work1", Ok(vec![window("5h", 12.0)]), 1000).unwrap();
        let mut meta = snapshot.meta(1000, 300);
        let account_ref = json!({ "id": "ca-work1", "label": "Work", "kind": "subscription" });
        meta["claudeAccount"] = account_ref.clone();
        let params = event_params(&account("claude"), &snapshot.gauges(1000, 300), &meta, account_ref);
        super::super::contract_validation::validate_cutex_event_message(&json!({
            "method": super::super::profile_quota_native::PROFILE_QUOTA_UPDATED_EVENT,
            "params": params,
        }))
        .expect("valid Claude quota event");
    }

    #[test]
    fn expired_credentials_keep_the_last_reading_without_counting_a_failure() {
        let dir = temp_dir();
        apply_read_at(&dir, "ca-work1", Ok(vec![window("5h", 12.0)]), 1000).unwrap();
        apply_read_at(&dir, "ca-work1", Err("read failed: HTTP 500".into()), 1100).unwrap();
        let (stale, changed) =
            apply_read_at(&dir, "ca-work1", ReadOutcome::StaleCredentials { expires_at_ms: 1_050_500 }, 1200).unwrap();
        assert!(changed);
        assert_eq!(stale.fetched_at, Some(1000));
        assert_eq!(stale.windows.len(), 1);
        assert_eq!(stale.last_error, None);
        assert_eq!(stale.consecutive_failures, 0);
        assert_eq!(stale.credentials_expired_at, Some(1050));
        assert!(refresh_due(Some(&stale), 1500, 300));
        let meta = stale.meta(1200, 300);
        assert_eq!(meta["credentialsExpired"], true);
        assert_eq!(meta["credentialsExpiredAt"], 1050);
        assert_eq!(meta["lastError"], Value::Null);
        assert!(!stale.gauges(1200, 300)[0].stale);

        let (same, changed) =
            apply_read_at(&dir, "ca-work1", ReadOutcome::StaleCredentials { expires_at_ms: 1_050_500 }, 1500).unwrap();
        assert!(!changed);
        assert_eq!(same.last_attempt_at, Some(1500));

        let (fresh, changed) = apply_read_at(&dir, "ca-work1", Ok(vec![window("5h", 20.0)]), 1800).unwrap();
        assert!(changed);
        assert_eq!(fresh.credentials_expired_at, None);
        assert_eq!(fresh.meta(1800, 300)["credentialsExpired"], false);
        // A snapshot without the field (before r224) still loads.
        let old: ClaudeQuotaSnapshot = serde_json::from_value(json!({"version": 1, "fetchedAt": 5})).unwrap();
        assert_eq!(old.credentials_expired_at, None);
    }

    #[test]
    fn refresh_backs_off_after_failures() {
        let mut snapshot = ClaudeQuotaSnapshot { last_attempt_at: Some(0), ..Default::default() };
        assert!(refresh_due(None, 0, 300));
        assert!(!refresh_due(Some(&snapshot), 299, 300));
        assert!(refresh_due(Some(&snapshot), 300, 300));
        snapshot.consecutive_failures = 2;
        assert!(!refresh_due(Some(&snapshot), 1199, 300));
        assert!(refresh_due(Some(&snapshot), 1200, 300));
    }

    #[test]
    fn applies_to_claude_profiles_without_quota_config() {
        let mut claude = account("claude");
        assert!(claude_quota_applies(&claude));
        claude.options.quota.enabled = true;
        assert!(!claude_quota_applies(&claude));
        assert!(!claude_quota_applies(&account("codex")));
    }

    #[test]
    fn refresher_reads_each_recently_used_subscription_account_once() {
        let now = chrono::Utc::now();
        let used = |name: &str, claude_account: Option<&str>, recent: bool| {
            let mut profile = account("claude");
            profile.name = name.into();
            profile.claude_account = claude_account.map(str::to_string);
            profile.last_used_at = Some(if recent { now } else { now - chrono::Duration::days(3) });
            profile
        };
        let registry: claude_accounts::ClaudeAccountsFile = serde_json::from_value(json!({
            "version": 1,
            "accounts": [
                {"id": "ca-work1", "label": "Work", "kind": "subscription", "createdAt": "2026-10-09T00:00:00Z"},
                {"id": "ca-api01", "label": "DeepSeek", "kind": "api", "createdAt": "2026-10-09T00:00:00Z",
                 "api": {"baseUrl": "https://api.deepseek.com/anthropic"}},
            ]
        }))
        .unwrap();
        let profiles = vec![
            used("a", None, true),
            used("b", None, true),
            used("c", Some("ca-work1"), true),
            used("d", Some("ca-api01"), true),
            used("e", Some("ca-old99"), false),
        ];
        assert_eq!(accounts_to_refresh(&profiles, &registry, now.timestamp()), ["default", "ca-work1"]);
    }
}
