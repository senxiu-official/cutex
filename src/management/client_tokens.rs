//! Management client tokens: revocable, per-client bearers for remote
//! Management clients (cutex-desktop over an SSH port-forward).
//!
//! - Format: `cxm1.<43 chars base64url(32 random bytes)>`. The secret is shown
//!   once, when it is issued (`cutex management client-token issue`).
//! - Storage: `<cutex home>/management-clients/tokens.json` keeps only a
//!   domain-separated SHA-256 of each token (`tokenSha256`); the directory is
//!   owner-private (Windows: private DACL; Unix: 0700, files 0600).
//! - Rights (v1): a verified client token authenticates as the Management
//!   root principal on the Management API. It is never valid on the Agent Bus
//!   (the bus only knows its own root token and per-agent credentials).
//! - The daemon re-reads the store when its modification time or size
//!   changes, so `issue` / `revoke` from the CLI apply without a restart.
//! - `lastUsedAt` lives in a separate daemon-written file (`usage.json`,
//!   persisted at most once a minute per token), so CLI writes to
//!   `tokens.json` never race with request-path writes.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const TOKEN_PREFIX: &str = "cxm1.";
pub const TOKEN_ID_PREFIX: &str = "mct-";
const STORE_SCHEMA: &str = "cutex.management-client-tokens.v1";
const HASH_LABEL: &[u8] = b"cutex-management-client-token-v1\0";
const MAX_LABEL_CHARS: usize = 100;
/// Owner-given display names (`PATCH /v2/client-tokens/{id}`).
pub const MAX_OWNER_NAME_CHARS: usize = 60;
const MAX_TOKENS: usize = 100;
const USAGE_PERSIST_INTERVAL: Duration = Duration::from_secs(60);
/// Re-read the store at least this often even when mtime and size look unchanged.
const CACHE_MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTokenRecord {
    pub token_id: String,
    pub label: String,
    /// Hex SHA-256 of `HASH_LABEL | token`.
    pub token_sha256: String,
    pub created_at: String,
    /// `"device"` for a paired device (`POST /v2/pair`); absent = `"client"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Display name the owner gave this token (`PATCH
    /// /v2/client-tokens/{id}`); absent = none. Separate from `deviceName`,
    /// which the device itself reported at pairing. Older cutex builds ignore
    /// the field when reading and drop it on their next write of the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_name: Option<String>,
}

pub const KIND_CLIENT: &str = "client";
pub const KIND_DEVICE: &str = "device";

/// A listed token: never contains the secret or its hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTokenSummary {
    pub token_id: String,
    pub label: String,
    pub kind: String,
    pub device_name: Option<String>,
    pub platform: Option<String>,
    /// Owner-given display name (capability `client-token-rename`).
    pub owner_name: Option<String>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub last_address: Option<String>,
}

/// The device part of a `POST /v2/pair` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    pub token_id: String,
    pub kind: String,
    pub label: String,
    pub device_name: String,
    pub platform: String,
    pub created_at: String,
}

/// Bumped on every in-process revoke so open streams re-check their token at
/// once (CLI revokes from another process are seen by the periodic check).
static REVOCATION_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn revocation_generation() -> u64 {
    REVOCATION_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

/// The one-time issue result (the only place the secret appears).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedClientToken {
    pub token_id: String,
    pub label: String,
    pub token: String,
    pub created_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    schema: String,
    #[serde(default)]
    tokens: Vec<ClientTokenRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageFile {
    #[serde(default)]
    last_used: BTreeMap<String, String>,
    #[serde(default)]
    last_address: BTreeMap<String, String>,
}

/// The store directory (`tokens.json`, its lock and `usage.json`).
#[derive(Clone, Debug)]
pub struct ClientTokenStore {
    dir: PathBuf,
}

impl ClientTokenStore {
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::at(crate::config::paths::config_dir()?.join("management-clients")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn tokens_path(&self) -> PathBuf {
        self.dir.join("tokens.json")
    }

    fn usage_path(&self) -> PathBuf {
        self.dir.join("usage.json")
    }

    fn ensure_dir(&self) -> anyhow::Result<()> {
        ensure_private_dir(&self.dir)
    }

    fn with_store<T>(
        &self,
        mutate: impl FnOnce(&mut StoreFile) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.ensure_dir()?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("tokens.json.lock"))
            .context("open the client token store lock")?;
        lock.lock_exclusive().context("lock the client token store")?;
        let mut store = read_store(&self.tokens_path())?;
        let before = serde_json::to_vec(&store.tokens)?;
        let result = mutate(&mut store)?;
        if serde_json::to_vec(&store.tokens)? != before {
            store.schema = STORE_SCHEMA.to_string();
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.tokens_path(),
                &store,
                "management client tokens",
            )?;
        }
        Ok(result)
    }

    /// Create a token. The returned secret is not stored anywhere.
    pub fn issue(&self, label: &str) -> anyhow::Result<IssuedClientToken> {
        self.issue_record(label, None, None, None)
    }

    /// Create a device token (pairing). Same format and rights as a client
    /// token; listed with `kind: "device"`.
    pub fn issue_device(
        &self,
        label: &str,
        device_name: &str,
        platform: &str,
    ) -> anyhow::Result<(IssuedClientToken, PairedDevice)> {
        let device_name = validate_label(device_name)?;
        let issued = self.issue_record(
            label,
            Some(KIND_DEVICE),
            Some(device_name.clone()),
            Some(platform.to_string()),
        )?;
        let device = PairedDevice {
            token_id: issued.token_id.clone(),
            kind: KIND_DEVICE.to_string(),
            label: issued.label.clone(),
            device_name,
            platform: platform.to_string(),
            created_at: issued.created_at.clone(),
        };
        Ok((issued, device))
    }

    fn issue_record(
        &self,
        label: &str,
        kind: Option<&str>,
        device_name: Option<String>,
        platform: Option<String>,
    ) -> anyhow::Result<IssuedClientToken> {
        let label = validate_label(label)?;
        let token = generate_token();
        let token_sha256 = token_hash_hex(&token);
        let created_at = crate::management::instance::now_utc();
        let token_id = format!("{TOKEN_ID_PREFIX}{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let record = ClientTokenRecord {
            token_id: token_id.clone(),
            label: label.clone(),
            token_sha256,
            created_at: created_at.clone(),
            kind: kind.map(str::to_string),
            device_name,
            platform,
            owner_name: None,
        };
        self.with_store(|store| {
            if store.tokens.len() >= MAX_TOKENS {
                anyhow::bail!("at most {MAX_TOKENS} client tokens; revoke unused ones first");
            }
            store.tokens.push(record);
            Ok(())
        })?;
        Ok(IssuedClientToken {
            token_id,
            label,
            token,
            created_at,
        })
    }

    /// Remove a token. `Ok(false)` when no such token exists.
    pub fn revoke(&self, token_id: &str) -> anyhow::Result<bool> {
        self.with_store(|store| {
            let before = store.tokens.len();
            store.tokens.retain(|record| record.token_id != token_id);
            Ok(store.tokens.len() != before)
        })
        .inspect(|revoked| {
            if *revoked {
                REVOCATION_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                crate::browser_relay::credential_revoked(token_id);
            }
        })
    }

    /// Set (`Some`) or clear (`None`) a token's owner-given display name.
    /// The name is validated by `validate_owner_name` first. `Ok(None)` when
    /// no such token exists; otherwise the stored name.
    pub fn rename(
        &self,
        token_id: &str,
        name: Option<&str>,
    ) -> anyhow::Result<Option<Option<String>>> {
        self.with_store(|store| {
            let Some(record) = store.tokens.iter_mut().find(|record| record.token_id == token_id)
            else {
                return Ok(None);
            };
            record.owner_name = name.map(str::to_string);
            Ok(Some(record.owner_name.clone()))
        })
    }

    pub fn list(&self) -> anyhow::Result<Vec<ClientTokenSummary>> {
        let store = read_store(&self.tokens_path())?;
        let usage = read_usage(&self.usage_path());
        Ok(store
            .tokens
            .into_iter()
            .map(|record| ClientTokenSummary {
                last_used_at: usage
                    .last_used
                    .get(&record.token_id)
                    .map(|at| crate::management::instance::normalize_utc(at)),
                last_address: usage.last_address.get(&record.token_id).cloned(),
                kind: record.kind.unwrap_or_else(|| KIND_CLIENT.to_string()),
                device_name: record.device_name,
                platform: record.platform,
                owner_name: record.owner_name,
                token_id: record.token_id,
                label: record.label,
                created_at: crate::management::instance::normalize_utc(&record.created_at),
            })
            .collect())
    }

    /// Verify a presented bearer value; returns the token id on a match.
    /// Reads the store fresh (the daemon path caches by mtime and size).
    pub fn verify(&self, presented: &str) -> anyhow::Result<Option<String>> {
        let store = read_store(&self.tokens_path())?;
        Ok(match_token(&store.tokens, presented))
    }

    /// Record a use of `token_id` (best effort, at most once a minute per
    /// token per process). Entries of revoked tokens are pruned.
    pub fn record_use(&self, token_id: &str) {
        self.record_use_from(token_id, None);
    }

    /// `record_use` plus the peer address (`lastAddress`); a changed address
    /// is persisted at once, otherwise at most once a minute.
    pub fn record_use_from(&self, token_id: &str, address: Option<&str>) {
        static LAST: OnceLock<Mutex<HashMap<(PathBuf, String), (Instant, Option<String>)>>> =
            OnceLock::new();
        let key = (self.dir.clone(), token_id.to_string());
        {
            let mut last = LAST
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.get(&key).is_some_and(|(at, previous)| {
                at.elapsed() < USAGE_PERSIST_INTERVAL
                    && (address.is_none() || previous.as_deref() == address)
            }) {
                return;
            }
            last.insert(key, (Instant::now(), address.map(str::to_string)));
        }
        if let Err(error) = self.write_usage(token_id, address) {
            eprintln!("management client token: could not record use: {error:#}");
        }
    }

    fn write_usage(&self, token_id: &str, address: Option<&str>) -> anyhow::Result<()> {
        static WRITE: Mutex<()> = Mutex::new(());
        let _guard = WRITE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let known: Vec<String> = read_store(&self.tokens_path())?
            .tokens
            .into_iter()
            .map(|record| record.token_id)
            .collect();
        let mut usage = read_usage(&self.usage_path());
        usage.last_used.retain(|id, _| known.contains(id));
        usage.last_address.retain(|id, _| known.contains(id));
        if known.iter().any(|id| id == token_id) {
            usage
                .last_used
                .insert(token_id.to_string(), crate::management::instance::now_utc());
            if let Some(address) = address {
                usage
                    .last_address
                    .insert(token_id.to_string(), address.to_string());
            }
        }
        self.ensure_dir()?;
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.usage_path(),
            &usage,
            "management client token usage",
        )
    }
}

/// True when the bearer value has the client-token shape (cheap pre-check
/// so ordinary requests never touch the store).
pub fn looks_like_client_token(presented: &str) -> bool {
    presented.starts_with(TOKEN_PREFIX)
}

/// Verify against the store, re-reading it only when its modification time
/// or length changed. Errors (unreadable or corrupt store) verify nothing.
fn verify_cached_at(store: &ClientTokenStore, presented: &str) -> Option<String> {
    struct Cached {
        path: PathBuf,
        stamp: Option<(SystemTime, u64)>,
        loaded_at: Instant,
        tokens: Vec<ClientTokenRecord>,
    }
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    let path = store.tokens_path();
    let stamp = std::fs::metadata(&path)
        .ok()
        .map(|meta| (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len()));
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fresh = cache
        .as_ref()
        .is_some_and(|cached| {
            cached.path == path
                && cached.stamp == stamp
                && stamp.is_some()
                && cached.loaded_at.elapsed() < CACHE_MAX_AGE
        });
    if !fresh {
        let tokens = match read_store(&path) {
            Ok(store) => store.tokens,
            Err(error) => {
                eprintln!("management client tokens unavailable: {error:#}");
                *cache = None;
                return None;
            }
        };
        *cache = Some(Cached {
            path,
            stamp,
            loaded_at: Instant::now(),
            tokens,
        });
    }
    match_token(&cache.as_ref()?.tokens, presented)
}

/// Authenticate a Management request that presents a client token.
///
/// Management checks compare the bearer with the root credential; a verified
/// client token is therefore replaced (in process only) with the root bearer,
/// so every existing root check applies unchanged and nothing downstream has
/// to know about client tokens. A client-token-shaped bearer that does not
/// verify (or arrives while no root credential is configured, so client
/// tokens cannot be root-equivalent) is removed.
pub fn authenticate_management_request(
    request: &mut crate::http::server::SimpleHttpRequest,
    root: Option<&str>,
) -> Option<VerifiedClientToken> {
    if !presents_client_token(request) {
        return None;
    }
    match ClientTokenStore::open_default() {
        Ok(store) => authenticate_management_request_with(request, root, &store),
        Err(_) => {
            request.headers.remove("authorization");
            None
        }
    }
}

/// A client/device token that authenticated the current request. Streams
/// keep it to notice a later revoke.
#[derive(Clone, Debug)]
pub struct VerifiedClientToken {
    pub token_id: String,
    presented: String,
    /// The user the token acted as when it was verified (P1 users).
    pub principal: crate::management::users::Principal,
}

impl VerifiedClientToken {
    /// Uncached validation for a browser admission/dispatch/result boundary.
    /// A separate CLI may have removed the token without changing our atomic.
    pub fn still_valid_uncached(&self) -> bool {
        ClientTokenStore::open_default().ok()
            .and_then(|store| read_store(&store.tokens_path()).ok())
            .and_then(|store| match_token(&store.tokens, &self.presented))
            .is_some_and(|id| id == self.token_id)
            && self.same_access_in(None)
    }
    /// Still present in the (cached, at most 2 s old) default store, and
    /// still acting as the same user with the same grant. A stream whose
    /// token was rebound or whose grant changed ends (`cutex/access/changed`).
    pub fn still_valid(&self) -> bool {
        ClientTokenStore::open_default()
            .ok()
            .and_then(|store| verify_cached_at(&store, &self.presented))
            .is_some_and(|id| id == self.token_id)
            && self.same_access_in(None)
    }

    fn same_access_in(&self, store: Option<&ClientTokenStore>) -> bool {
        let users = match store {
            Some(store) => crate::management::users::UserStore::beside(store),
            None => match crate::management::users::UserStore::open_default() {
                Ok(users) => users,
                Err(_) => return false,
            },
        };
        users.principal_for_token(&self.token_id).access_stamp() == self.principal.access_stamp()
    }

    #[cfg(test)]
    pub fn still_valid_in(&self, store: &ClientTokenStore) -> bool {
        verify_cached_at(store, &self.presented).is_some_and(|id| id == self.token_id)
            && self.same_access_in(Some(store))
    }
}

/// True when the request carries a client-token-shaped bearer.
pub fn presents_client_token(request: &crate::http::server::SimpleHttpRequest) -> bool {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(looks_like_client_token)
}

pub fn authenticate_management_request_with(
    request: &mut crate::http::server::SimpleHttpRequest,
    root: Option<&str>,
    store: &ClientTokenStore,
) -> Option<VerifiedClientToken> {
    authenticate_management_request_from(request, root, store, None)
}

/// As `authenticate_management_request_with`, recording the peer address.
pub fn authenticate_management_request_from(
    request: &mut crate::http::server::SimpleHttpRequest,
    root: Option<&str>,
    store: &ClientTokenStore,
    peer: Option<&str>,
) -> Option<VerifiedClientToken> {
    if !presents_client_token(request) {
        return None;
    }
    let presented = request
        .headers
        .remove("authorization")
        .and_then(|value| value.strip_prefix("Bearer ").map(str::to_string))
        .unwrap_or_default();
    let root = root.filter(|root| !root.trim().is_empty())?;
    let token_id = verify_cached_at(store, &presented)?;
    let principal = crate::management::users::UserStore::beside(store).principal_for_token(&token_id);
    request
        .headers
        .insert("authorization".into(), format!("Bearer {root}"));
    store.record_use_from(&token_id, peer);
    Some(VerifiedClientToken {
        token_id,
        presented,
        principal,
    })
}

fn match_token(tokens: &[ClientTokenRecord], presented: &str) -> Option<String> {
    if !looks_like_client_token(presented) {
        return None;
    }
    let presented_hash = token_hash_hex(presented);
    let mut found = None;
    // Compare against every record in constant time per record.
    for record in tokens {
        if constant_time_eq(presented_hash.as_bytes(), record.token_sha256.as_bytes()) {
            found = Some(record.token_id.clone());
        }
    }
    found
}

pub fn token_hash_hex(token: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(HASH_LABEL);
    digest.update(token.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// 32 bytes from the OS RNG (via uuid v4, which draws from `getrandom`),
/// whitened through SHA-256 over three independent v4 values (366 bits).
fn generate_token() -> String {
    let mut digest = Sha256::new();
    digest.update(b"cutex-management-client-token-gen\0");
    for _ in 0..3 {
        digest.update(uuid::Uuid::new_v4().as_bytes());
    }
    let bytes: [u8; 32] = digest.finalize().into();
    format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn validate_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    if label.is_empty() {
        anyhow::bail!("a non-empty --label is required");
    }
    if label.chars().count() > MAX_LABEL_CHARS || label.chars().any(char::is_control) {
        anyhow::bail!("label: at most {MAX_LABEL_CHARS} printable characters");
    }
    Ok(label.to_string())
}

/// An owner-given display name: trimmed, 1..=60 characters, no control
/// characters. `Err` carries a client-facing message.
pub fn validate_owner_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("name must not be empty (send null to clear it)".into());
    }
    if name.chars().count() > MAX_OWNER_NAME_CHARS {
        return Err(format!("name: at most {MAX_OWNER_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters".into());
    }
    Ok(name.to_string())
}

fn read_store(path: &Path) -> anyhow::Result<StoreFile> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("client token store is corrupt: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn read_usage(path: &Path) -> UsageFile {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Create `dir` (and parents) and make it owner-private: a private DACL on
/// Windows, mode 0700 on Unix.
pub fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(windows)]
    crate::platform::private_fs::secure_directory(dir)
        .map_err(|error| anyhow::anyhow!("secure {}: {error:?}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> ClientTokenStore {
        ClientTokenStore::at(
            std::env::temp_dir()
                .join(format!("cutex-client-tokens-{}", uuid::Uuid::new_v4()))
                .join("management-clients"),
        )
    }

    #[test]
    fn issued_token_verifies_and_only_its_hash_is_stored() {
        let store = temp_store();
        let issued = store.issue("desktop-eva").unwrap();
        assert!(issued.token.starts_with(TOKEN_PREFIX));
        assert_eq!(issued.token.len(), TOKEN_PREFIX.len() + 43);
        assert!(issued.token_id.starts_with(TOKEN_ID_PREFIX));
        assert_eq!(store.verify(&issued.token).unwrap(), Some(issued.token_id.clone()));
        let raw = std::fs::read_to_string(store.tokens_path()).unwrap();
        assert!(!raw.contains(&issued.token));
        assert!(!raw.contains(&issued.token[TOKEN_PREFIX.len()..]));
        assert!(raw.contains(&token_hash_hex(&issued.token)));
        // Wrong, truncated or prefix-less values do not verify.
        let mut wrong = issued.token.clone();
        wrong.pop();
        wrong.push(if issued.token.ends_with('A') { 'B' } else { 'A' });
        assert_eq!(store.verify(&wrong).unwrap(), None);
        assert_eq!(store.verify(&issued.token[..issued.token.len() - 1]).unwrap(), None);
        assert_eq!(store.verify(&issued.token[TOKEN_PREFIX.len()..]).unwrap(), None);
        // Two tokens are distinct.
        let other = store.issue("desktop-two").unwrap();
        assert_ne!(other.token, issued.token);
        assert_eq!(store.verify(&other.token).unwrap(), Some(other.token_id));
    }

    #[test]
    fn list_has_no_secrets_and_revoke_invalidates() {
        let store = temp_store();
        let issued = store.issue("  laptop  ").unwrap();
        assert_eq!(issued.label, "laptop");
        store.record_use(&issued.token_id);
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].token_id, issued.token_id);
        assert!(listed[0].last_used_at.is_some());
        let json = serde_json::to_string(&listed).unwrap();
        assert!(!json.contains(&issued.token));
        assert!(!json.contains("Sha256"));
        assert!(store.revoke(&issued.token_id).unwrap());
        assert!(!store.revoke(&issued.token_id).unwrap());
        assert_eq!(store.verify(&issued.token).unwrap(), None);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn cached_verification_sees_revocation_without_restart() {
        let store = temp_store();
        let issued = store.issue("cache").unwrap();
        assert_eq!(verify_cached_at(&store, &issued.token), Some(issued.token_id.clone()));
        assert!(store.revoke(&issued.token_id).unwrap());
        assert_eq!(verify_cached_at(&store, &issued.token), None);
        let again = store.issue("cache-2").unwrap();
        assert_eq!(verify_cached_at(&store, &again.token), Some(again.token_id));
    }

    #[test]
    fn rename_sets_and_clears_the_owner_name_only() {
        let store = temp_store();
        let issued = store.issue("desktop-host-Cutex-abc123").unwrap();
        let (device, _) = store.issue_device("phone-label", "Pixel", "android").unwrap();
        assert_eq!(store.list().unwrap()[0].owner_name, None);
        assert_eq!(
            store.rename(&issued.token_id, Some("Work laptop")).unwrap(),
            Some(Some("Work laptop".to_string()))
        );
        assert_eq!(store.rename(&device.token_id, Some("My phone")).unwrap(), Some(Some("My phone".into())));
        let listed = store.list().unwrap();
        assert_eq!(listed[0].owner_name.as_deref(), Some("Work laptop"));
        assert_eq!(listed[0].label, "desktop-host-Cutex-abc123");
        assert_eq!(listed[1].device_name.as_deref(), Some("Pixel"));
        assert_eq!(listed[1].owner_name.as_deref(), Some("My phone"));
        let json = serde_json::to_value(&listed[0]).unwrap();
        assert_eq!(json["ownerName"], "Work laptop");
        // The token still verifies after a rename.
        assert_eq!(store.verify(&issued.token).unwrap(), Some(issued.token_id.clone()));
        assert_eq!(store.rename(&issued.token_id, None).unwrap(), Some(None));
        assert_eq!(store.list().unwrap()[0].owner_name, None);
        let raw = std::fs::read_to_string(store.tokens_path()).unwrap();
        assert!(!raw.contains("Work laptop"));
        assert!(raw.contains("My phone"));
        assert_eq!(store.rename("mct-0000000000000000", Some("x")).unwrap(), None);
    }

    #[test]
    fn records_without_owner_name_still_read_and_older_records_ignore_it() {
        // A store written by an older cutex (no ownerName) reads as None.
        let old = r#"{"schema":"cutex.management-client-tokens.v1","tokens":[{"tokenId":"mct-1","label":"l","tokenSha256":"00","createdAt":"2026-10-15T00:00:00.000Z"}]}"#;
        let parsed: StoreFile = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.tokens[0].owner_name, None);
        assert!(!serde_json::to_string(&parsed.tokens).unwrap().contains("ownerName"));
    }

    #[test]
    fn owner_names_are_validated() {
        assert_eq!(validate_owner_name("  Laptop  ").unwrap(), "Laptop");
        assert!(validate_owner_name("   ").is_err());
        assert!(validate_owner_name("a\tb").is_err());
        assert!(validate_owner_name("a\u{7f}").is_err());
        assert_eq!(validate_owner_name(&"界".repeat(60)).unwrap().chars().count(), 60);
        assert!(validate_owner_name(&"x".repeat(61)).is_err());
    }

    #[test]
    fn labels_are_validated() {
        let store = temp_store();
        assert!(store.issue("   ").is_err());
        assert!(store.issue("bad\nlabel").is_err());
        assert!(store.issue(&"x".repeat(101)).is_err());
    }

    #[test]
    fn agent_bus_does_not_accept_client_tokens() {
        let store = temp_store();
        let issued = store.issue("bus").unwrap();
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), format!("Bearer {}", issued.token));
        let caller =
            crate::agent_bus::agent_auth::authenticate(&headers, Some("agent-bus-root-token"))
                .unwrap();
        assert_eq!(caller, crate::agent_bus::agent_auth::BusCaller::Unauthenticated);
    }

    fn request_with_bearer(bearer: &str) -> crate::http::server::SimpleHttpRequest {
        crate::http::server::SimpleHttpRequest {
            method: "GET".into(),
            path: "/v2/capabilities".into(),
            headers: HashMap::from([("authorization".to_string(), format!("Bearer {bearer}"))]),
            body: Vec::new(),
        }
    }

    #[test]
    fn verified_client_token_authenticates_as_root_and_records_use() {
        let store = temp_store();
        let issued = store.issue("desktop").unwrap();
        let mut request = request_with_bearer(&issued.token);
        authenticate_management_request_with(&mut request, Some("root-secret"), &store);
        assert_eq!(request.headers["authorization"], "Bearer root-secret");
        assert!(store.list().unwrap()[0].last_used_at.is_some());
        // Revoked: the bearer is removed, so every check answers 401.
        store.revoke(&issued.token_id).unwrap();
        let mut request = request_with_bearer(&issued.token);
        authenticate_management_request_with(&mut request, Some("root-secret"), &store);
        assert!(!request.headers.contains_key("authorization"));
    }

    #[test]
    fn client_token_shaped_bearer_without_root_or_match_is_stripped() {
        let store = temp_store();
        let issued = store.issue("no-root").unwrap();
        let mut request = request_with_bearer(&issued.token);
        authenticate_management_request_with(&mut request, None, &store);
        assert!(!request.headers.contains_key("authorization"));
        let mut request = request_with_bearer("cxm1.not-a-real-token");
        authenticate_management_request_with(&mut request, Some("root-secret"), &store);
        assert!(!request.headers.contains_key("authorization"));
        // Ordinary bearers are untouched.
        let mut request = request_with_bearer("root-secret");
        authenticate_management_request_with(&mut request, Some("root-secret"), &store);
        assert_eq!(request.headers["authorization"], "Bearer root-secret");
    }
}
