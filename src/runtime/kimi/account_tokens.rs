//! cutex keeps a Kimi account's shared OAuth token fresh ahead of Kimi (r266).
//!
//! Every agent of a Kimi account links the account's `credentials/` and
//! `oauth/` (one token file). Kimi Code refreshes the token itself when less
//! than max(300 s, expires_in / 2) is left, under `proper-lockfile`'s
//! `oauth/<slot>.lock` with a re-read after the lock, but **not on Windows**
//! (Kimi 2.3.3 skips the lock there) and refresh tokens rotate: two unlocked
//! refreshes with one refresh token make the second fail (a stale-token 401
//! Kimi then recovers from, or, in the worst order, a tombstone).
//!
//! So cutex refreshes first: at every start of an account agent and on a
//! management-process tick while an agent of the account runs, it refreshes
//! once the token is within Kimi's threshold plus a headroom
//! ([`refresh_due`]), under a per-account in-process lock and the same
//! mkdir lock Kimi and K2's login / logout use (`RefreshLock`, cross-platform,
//! kept fresh while held), re-reading the file after each lock. Kimi then finds
//! a fresh token and does not refresh. Residual window (documented in
//! docs/development/r266-kimi-runtime-notes.md): when cutex's refresh fails
//! (network, OAuth host down) or the process that would tick is not running,
//! Kimi refreshes on its own, unlocked on Windows.
//!
//! Every value cutex sees is fingerprinted into the account's redaction ledger
//! ([`super::redaction`]). No function here returns, logs or formats a token.
//!
//! The refresh request mirrors the pinned Kimi Code 2.3.3 client
//! (`packages/oauth/src/oauth.ts` `refreshAccessToken`, `constants.ts`,
//! `identity.ts`): `POST {oauth_host}/api/oauth/token`, form
//! `client_id`, `grant_type=refresh_token`, `refresh_token`, Kimi's device
//! headers, 3 attempts with 1 s / 2 s backoff on transport errors, 429 and 5xx.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

use super::redaction;
use crate::profiles::kimi_accounts::{self, RefreshLock};

/// `KIMI_CODE_FLOW_CONFIG.clientId` of the pinned Kimi Code 2.3.3.
pub const KIMI_CODE_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
pub const KIMI_PLATFORM: &str = "kimi_code_cli";
pub const MAINLAND_OAUTH_HOST: &str = "https://auth.kimi.com";
pub const GLOBAL_OAUTH_HOST: &str = "https://auth.kimi.ai";
const MIN_REFRESH_MARGIN_S: i64 = 300;
/// cutex refreshes this much before Kimi would (at least two keeper ticks).
const MIN_HEADROOM_S: i64 = 120;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the management process checks the accounts of running agents.
pub const KEEPER_TICK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// Never logged in, logged out or the refresh was rejected: log in again.
    LoggedOut(String),
    /// The refresh failed for now (network, 5xx); the request may be retried.
    Unavailable(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LoggedOut(message) | Self::Unavailable(message) => f.write_str(message),
        }
    }
}

/// One Kimi account (from an agent's frozen binding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRef {
    pub account_id: String,
    pub home: PathBuf,
    pub slot: String,
}

struct Stored {
    access: String,
    refresh: String,
    expires_at: i64,
    expires_in: i64,
    raw: Value,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn read_stored(path: &Path) -> Option<Stored> {
    let raw: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let text = |key: &str| raw.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let number = |key: &str| raw.get(key).and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))).unwrap_or(0);
    Some(Stored { access: text("access_token"), refresh: text("refresh_token"), expires_at: number("expires_at"), expires_in: number("expires_in"), raw })
}

/// cutex refreshes when less than Kimi's own threshold
/// (`defaultRefreshThreshold`: max(300 s, expires_in / 2)) plus a headroom of
/// max(120 s, expires_in / 4) is left, so Kimi never reaches its own.
pub fn refresh_due(expires_at: i64, expires_in: i64, at: i64) -> bool {
    let kimi = MIN_REFRESH_MARGIN_S.max(expires_in / 2);
    expires_at - at < kimi + MIN_HEADROOM_S.max(expires_in / 4)
}

fn due(stored: &Stored, at: i64) -> bool {
    refresh_due(stored.expires_at, stored.expires_in, at)
}

fn account_locks() -> &'static Mutex<BTreeMap<String, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

fn account_lock(id: &str) -> Arc<Mutex<()>> {
    account_locks().lock().unwrap_or_else(|e| e.into_inner()).entry(id.to_string()).or_default().clone()
}

/// What [`ensure_fresh`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Far from expiry (or a peer refreshed while this waited for the lock).
    Fresh,
    /// cutex refreshed the token.
    Refreshed,
}

/// Refreshes the account's token when [`refresh_due`]: one refresh for any
/// number of concurrent callers (in-process lock, then the account's refresh
/// lock, re-reading the file after each).
pub fn ensure_fresh(account: &AccountRef) -> Result<Freshness, TokenError> {
    let path = kimi_accounts::credential_path(&account.home, &account.slot).map_err(|_| TokenError::LoggedOut("invalid Kimi credential slot".into()))?;
    let logged_out = || TokenError::LoggedOut(format!("the Kimi account {} is logged out; log in again in cutex", account.account_id));
    let usable = |stored: &Stored| !stored.access.is_empty() && !stored.refresh.is_empty();
    let check = || -> Result<Option<Stored>, TokenError> {
        let stored = read_stored(&path).ok_or_else(logged_out)?;
        if !usable(&stored) {
            return Err(logged_out());
        }
        Ok(due(&stored, now()).then_some(stored))
    };
    if check()?.is_none() {
        return Ok(Freshness::Fresh);
    }
    let lock = account_lock(&account.account_id);
    let _in_process = lock.lock().unwrap_or_else(|e| e.into_inner());
    if check()?.is_none() {
        return Ok(Freshness::Fresh);
    }
    let _file_lock = RefreshLock::acquire(&account.home, &account.slot).map_err(|error| TokenError::Unavailable(format!("{error:#}")))?;
    let Some(stored) = check()? else { return Ok(Freshness::Fresh) };
    match refresh(account, &stored.refresh) {
        Ok(fresh) => {
            let access = fresh.get("access_token").and_then(Value::as_str).unwrap_or("").to_string();
            let refresh_token = fresh.get("refresh_token").and_then(Value::as_str).unwrap_or("").to_string();
            if access.is_empty() || refresh_token.is_empty() {
                return Err(TokenError::Unavailable("the Kimi OAuth host answered without tokens".into()));
            }
            let expires_in = fresh.get("expires_in").and_then(Value::as_f64).unwrap_or(0.0);
            // Kimi's `tokenToWire`: the same fields, written atomically.
            let mut next = stored.raw.as_object().cloned().unwrap_or_default();
            next.insert("access_token".into(), json!(access));
            next.insert("refresh_token".into(), json!(refresh_token));
            next.insert("expires_at".into(), json!(now() + expires_in as i64));
            next.insert("expires_in".into(), json!(expires_in));
            for key in ["scope", "token_type"] {
                if let Some(value) = fresh.get(key).and_then(Value::as_str) {
                    next.insert(key.into(), json!(value));
                }
            }
            // Fingerprints first: a reader never sees a value the ledger lacks.
            redaction::remember(&account.account_id, [stored.access.as_str(), stored.refresh.as_str(), access.as_str(), refresh_token.as_str()]);
            let bytes = serde_json::to_vec_pretty(&Value::Object(next)).map_err(|_| TokenError::Unavailable("encode the refreshed Kimi credential".into()))?;
            crate::config::atomic::write_private_bytes_atomic(&path, &bytes).map_err(|error| TokenError::Unavailable(format!("store the refreshed Kimi credential: {error:#}")))?;
            Ok(Freshness::Refreshed)
        }
        Err(RefreshFailure::Unauthorized) => {
            // Kimi's own rule: a rejected refresh token is a logout (tombstone),
            // so no process of the account retries it.
            redaction::remember(&account.account_id, [stored.access.as_str(), stored.refresh.as_str()]);
            let tombstone = json!({
                "access_token": "", "refresh_token": "", "expires_at": 0, "expires_in": 0,
                "scope": stored.raw.get("scope").and_then(Value::as_str).unwrap_or(""),
                "token_type": stored.raw.get("token_type").and_then(Value::as_str).unwrap_or(""),
            });
            if let Ok(bytes) = serde_json::to_vec_pretty(&tombstone) {
                let _ = crate::config::atomic::write_private_bytes_atomic(&path, &bytes);
            }
            Err(TokenError::LoggedOut(format!("the Kimi account {} must log in again (the token refresh was rejected)", account.account_id)))
        }
        Err(RefreshFailure::Transient(message)) => Err(TokenError::Unavailable(format!("the Kimi token refresh failed: {message}"))),
    }
}

/// The values of the account's current credential (fingerprints only).
fn current_values(account: &AccountRef) -> Vec<String> {
    kimi_accounts::credential_path(&account.home, &account.slot)
        .ok()
        .and_then(|path| read_stored(&path))
        .map(|stored| [stored.access, stored.refresh].into_iter().filter(|value| !value.is_empty()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- refresh

enum RefreshFailure {
    Unauthorized,
    Transient(String),
}

/// The OAuth host of the account: its config's `oauth_host`, else the region's.
/// Release builds accept only Kimi's two official hosts; private debug tests may
/// point at a fake server (`CUTEX_KIMI_TEST_OAUTH_HOST`).
pub fn oauth_host(account: &AccountRef) -> anyhow::Result<String> {
    if let Some(host) = crate::profiles::kimi::test_override(crate::profiles::kimi::TEST_OAUTH_HOST_ENV) {
        return Ok(host.trim_end_matches('/').to_string());
    }
    let config = crate::profiles::kimi::read_config(&account.home).unwrap_or_default();
    let configured = config
        .get("providers")
        .and_then(|providers| providers.get(crate::profiles::kimi::MANAGED_PROVIDER))
        .and_then(|provider| provider.get("oauth"))
        .and_then(|oauth| oauth.get("oauth_host"))
        .and_then(toml::Value::as_str)
        .map(|host| host.trim_end_matches('/').to_string());
    let region = kimi_accounts::load().ok().and_then(|registry| registry.find(&account.account_id).map(|entry| entry.region));
    let host = configured.unwrap_or_else(|| match region {
        Some(kimi_accounts::KimiRegion::Global) => GLOBAL_OAUTH_HOST.to_string(),
        _ => MAINLAND_OAUTH_HOST.to_string(),
    });
    anyhow::ensure!(host == MAINLAND_OAUTH_HOST || host == GLOBAL_OAUTH_HOST, "the Kimi account's OAuth host is not an official Kimi host");
    Ok(host)
}

fn ascii_header(value: &str) -> String {
    let cleaned: String = value.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()).collect();
    if cleaned.trim().is_empty() { "unknown".into() } else { cleaned.trim().to_string() }
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|name| ascii_header(&name))
        .unwrap_or_else(|| "unknown".into())
}

fn refresh(account: &AccountRef, refresh_token: &str) -> Result<Value, RefreshFailure> {
    let host = oauth_host(account).map_err(|error| RefreshFailure::Transient(format!("{error:#}")))?;
    let url = format!("{host}/api/oauth/token");
    let device_id = std::fs::read_to_string(account.home.join("device_id")).map(|id| ascii_header(&id)).unwrap_or_else(|_| "unknown".into());
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", KIMI_CODE_CLIENT_ID)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .finish();
    let agent = crate::config::proxy_outbound::OutboundProxy::host()
        .apply(ureq::Agent::config_builder().timeout_global(Some(REQUEST_TIMEOUT)).max_redirects(0).http_status_as_error(false))
        .map_err(|error| RefreshFailure::Transient(error.to_string()))?
        .build()
        .new_agent();
    let mut last = String::from("no attempt");
    for attempt in 0..3u32 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
        }
        let response = agent
            .post(&url)
            .header("X-Msh-Platform", KIMI_PLATFORM)
            .header("X-Msh-Version", super::PINNED_KIMI_VERSION)
            .header("X-Msh-Device-Name", &host_name())
            .header("X-Msh-Device-Model", &ascii_header(&format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)))
            .header("X-Msh-Os-Version", &ascii_header(std::env::consts::OS))
            .header("X-Msh-Device-Id", &device_id)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .send(body.as_bytes());
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                last = crate::config::proxy_outbound::proxy_error_message(&error).unwrap_or_else(|| "the Kimi OAuth host could not be reached".into());
                continue;
            }
        };
        let status = response.status().as_u16();
        let data: Value = response.body_mut().with_config().limit(1024 * 1024).read_json().unwrap_or(Value::Null);
        if status == 200 && data.get("access_token").and_then(Value::as_str).is_some() {
            return Ok(data);
        }
        let code = data.get("error").and_then(Value::as_str).unwrap_or("");
        if status == 401 || status == 403 || code == "invalid_grant" {
            return Err(RefreshFailure::Unauthorized);
        }
        last = format!("HTTP {status}");
        if !(status == 429 || status >= 500) {
            break;
        }
    }
    Err(RefreshFailure::Transient(last))
}

// ---------------------------------------------------------------- keeper

/// Records the account's current values in its redaction ledger.
pub fn remember_current(account: &AccountRef) {
    let values = current_values(account);
    redaction::remember(&account.account_id, values.iter().map(String::as_str));
}

/// The accounts of Kimi agents that have a live runtime binding.
fn running_accounts() -> Vec<AccountRef> {
    let Ok(store) = crate::session::store::load_cutex_session_store() else { return Vec::new() };
    let mut seen = BTreeSet::new();
    store
        .sessions
        .values()
        .filter(|record| record.runtime_backend == crate::session::model::CutexSessionRuntimeBackend::Kimi && record.app_server_runtime.is_some())
        .filter_map(super::credential::bound_account)
        .filter(|account| seen.insert(account.account_id.clone()))
        .collect()
}

fn keeper_tick() -> Duration {
    crate::profiles::kimi::test_override("CUTEX_KIMI_TEST_KEEPER_TICK_MS")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(KEEPER_TICK)
}

/// The management process's keeper: every tick, the accounts of running Kimi
/// agents are refreshed when due and their values fingerprinted.
pub fn spawn_keeper() -> anyhow::Result<()> {
    std::thread::Builder::new().name("cutex-kimi-tokens".into()).spawn(|| loop {
        std::thread::sleep(keeper_tick());
        for account in running_accounts() {
            remember_current(&account);
            match ensure_fresh(&account) {
                Ok(Freshness::Refreshed) => remember_current(&account),
                Ok(Freshness::Fresh) => {}
                Err(error) => eprintln!("kimi: the account {} token was not refreshed: {error}", account.account_id),
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::kimi_accounts::tests::{logged_in_account, EnvGuard};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake Kimi OAuth host: rotating refresh tokens (a superseded one gets
    /// 401 invalid_grant, as Kimi's does), a delay that widens any race.
    struct FakeOAuth {
        port: u16,
        refreshes: Arc<AtomicUsize>,
        rejected: Arc<AtomicUsize>,
    }

    fn fake_oauth(first_refresh: &str, delay: Duration) -> FakeOAuth {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let rejected = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(Mutex::new(first_refresh.to_string()));
        let (count, refused) = (refreshes.clone(), rejected.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (count, refused, current) = (count.clone(), refused.clone(), current.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length];
                    let _ = reader.read_exact(&mut body);
                    let form: BTreeMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                    std::thread::sleep(delay);
                    let mut current = current.lock().unwrap();
                    let (status, answer) = if form.get("refresh_token") == Some(&*current) {
                        let generation = count.fetch_add(1, Ordering::SeqCst) + 2;
                        *current = format!("fake-refresh-gen{generation}-0123456789");
                        (200, json!({"access_token": format!("fake-access-gen{generation}-0123456789"), "refresh_token": *current, "expires_in": 3600, "scope": "kimi-code", "token_type": "Bearer"}))
                    } else {
                        refused.fetch_add(1, Ordering::SeqCst);
                        (401, json!({"error": "invalid_grant"}))
                    };
                    let text = answer.to_string();
                    let _ = write!(stream, "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
                });
            }
        });
        FakeOAuth { port, refreshes, rejected }
    }

    fn near_expiry(home: &Path, access: &str, refresh: &str) {
        let token = json!({"access_token": access, "refresh_token": refresh, "expires_at": now() + 30, "scope": "kimi-code", "token_type": "Bearer", "expires_in": 3600});
        std::fs::write(home.join("credentials/kimi-code.json"), token.to_string()).unwrap();
    }

    #[test]
    fn cutex_refreshes_before_kimi_would() {
        // Kimi refreshes below max(300, expires_in / 2); cutex earlier by max(120, expires_in / 4).
        assert!(!refresh_due(10_000 + 3600, 3600, 10_000));
        assert!(!refresh_due(10_000 + 2701, 3600, 10_000));
        assert!(refresh_due(10_000 + 2699, 3600, 10_000), "1800 + 900");
        assert!(refresh_due(10_000 + 419, 600, 10_000), "300 + 150 (at least Kimi's 300 + 120)");
        assert!(!refresh_due(10_000 + 451, 600, 10_000));
        assert!(refresh_due(10_000 + 419, 0, 10_000), "unknown lifetime: 300 + 120");
    }

    #[test]
    fn concurrent_agents_of_one_account_refresh_once_and_every_value_is_fingerprinted() {
        let home_guard = crate::profiles::lifecycle::tests::TestHome::new();
        let fake = fake_oauth("fake-refresh-gen1-0123456789", Duration::from_millis(400));
        let host = format!("http://127.0.0.1:{}", fake.port);
        let _env = EnvGuard::set(&[("CUTEX_TEST_PRIVATE_HOME", home_guard.root().to_str().unwrap()), (crate::profiles::kimi::TEST_OAUTH_HOST_ENV, &host)]);
        let account = logged_in_account("Refresh");
        let home = kimi_accounts::account_home(&account.id).unwrap();
        near_expiry(&home, "fake-access-gen1-0123456789", "fake-refresh-gen1-0123456789");
        let reference = AccountRef { account_id: account.id.clone(), home: home.clone(), slot: "kimi-code".into() };
        // Four agents start at once (and a Kimi-side holder of the lock would wait too).
        let outcomes: Vec<_> = (0..4)
            .map(|_| {
                let reference = reference.clone();
                std::thread::spawn(move || ensure_fresh(&reference))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(outcomes.iter().filter(|outcome| **outcome == Ok(Freshness::Refreshed)).count(), 1, "{outcomes:?}");
        assert!(outcomes.iter().all(|outcome| outcome.is_ok()), "{outcomes:?}");
        assert_eq!((fake.refreshes.load(Ordering::SeqCst), fake.rejected.load(Ordering::SeqCst)), (1, 0));
        let raw = std::fs::read_to_string(home.join("credentials/kimi-code.json")).unwrap();
        assert!(raw.contains("fake-refresh-gen2-0123456789") && raw.contains("\"scope\""), "{raw}");
        assert_eq!(kimi_accounts::account_credential_state(&account.id).unwrap().state_name(), "loggedIn");
        assert!(!home.join("oauth/kimi-code.lock").exists(), "lock released");
        // Fresh now: no further refresh.
        assert_eq!(ensure_fresh(&reference), Ok(Freshness::Fresh));
        // The ledger holds both generations as digests.
        let ledger = redaction::ledger(&account.id);
        for value in ["fake-access-gen1-0123456789", "fake-refresh-gen1-0123456789", "fake-access-gen2-0123456789", "fake-refresh-gen2-0123456789"] {
            assert!(ledger.contains(&redaction::Fingerprint::of(value).unwrap()), "{value}");
        }
        // A rejected refresh token is Kimi's logout: a tombstone, never retried.
        near_expiry(&home, "fake-access-stale-0123456789", "fake-refresh-stale-0123456789");
        assert!(matches!(ensure_fresh(&reference), Err(TokenError::LoggedOut(_))));
        assert_eq!(kimi_accounts::account_credential_state(&account.id).unwrap().state_name(), "revoked");
        assert!(matches!(ensure_fresh(&reference), Err(TokenError::LoggedOut(_))));
        assert_eq!(fake.rejected.load(Ordering::SeqCst), 1);
    }
}
