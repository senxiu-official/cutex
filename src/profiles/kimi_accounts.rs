//! r266 Kimi accounts (docs/development/r266-kimi-profiles.md §3, §4).
//!
//! A Kimi account is one private `KIMI_CODE_HOME` owned by cutex,
//! `<config>/kimi-accounts/<id>/`, where the official `kimi login` (device
//! code) ran once. Agents of the account get their own `KIMI_CODE_HOME` whose
//! `credentials/` and `oauth/` are directory links to the account home's
//! ([`crate::profiles::kimi_launch`]), so every Kimi process of the account
//! reads and rotates one credential file under one refresh lock.
//!
//! cutex reads the credential file for presence / expiry / tombstone only and
//! copies its bytes once from the login staging home. No function here returns,
//! logs or formats a token.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, ensure, Context};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::atomic::{write_private_bytes_atomic, write_private_pretty_json_atomic};
use crate::profiles::model::{AccountsStore, CliKind};

pub const HOMES_DIR: &str = "kimi-accounts";
/// Staging homes of running logins (under [`HOMES_DIR`]).
pub const STAGING_DIR: &str = ".login";
pub const MAX_LABEL_CHARS: usize = 64;
const FILE_VERSION: u32 = 1;
const ID_PREFIX: &str = "ka-";
const LOGIN_ID_PREFIX: &str = "kl-";
/// Kimi's own device-code budget is 15 min; cutex stops the process after 16.
const LOGIN_DEADLINE: Duration = Duration::from_secs(16 * 60);
/// How long `POST …/login` waits for the device code before answering `starting`.
pub const LOGIN_CODE_WAIT: Duration = Duration::from_secs(20);
/// Finished logins are forgotten after this.
const LOGIN_RETENTION: Duration = Duration::from_secs(10 * 60);
const MAX_STDERR_LINE: usize = 2048;
/// `proper-lockfile` default staleness Kimi uses (`stale: 5_000`).
const LOCK_STALE: Duration = Duration::from_secs(5);
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
/// Default credential slot (mainland-cn endpoints).
pub const DEFAULT_SLOT: &str = "kimi-code";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KimiRegion {
    #[default]
    #[serde(rename = "mainland-cn")]
    MainlandCn,
    #[serde(rename = "global")]
    Global,
}

impl KimiRegion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MainlandCn => "mainland-cn",
            Self::Global => "global",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mainland-cn" => Some(Self::MainlandCn),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KimiAccount {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub region: KimiRegion,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_login_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KimiAccountsFile {
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<KimiAccount>,
}

impl Default for KimiAccountsFile {
    fn default() -> Self {
        Self { version: FILE_VERSION, accounts: Vec::new() }
    }
}

impl KimiAccountsFile {
    pub fn find(&self, id: &str) -> Option<&KimiAccount> {
        self.accounts.iter().find(|account| account.id == id)
    }
}

// ---------------------------------------------------------------------------
// Paths and persistence
// ---------------------------------------------------------------------------

pub fn registry_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("kimi/accounts.json"))
}

fn lock_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("kimi/accounts.lock"))
}

pub fn homes_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(HOMES_DIR))
}

/// `KIMI_CODE_HOME` of an account.
pub fn account_home(id: &str) -> anyhow::Result<PathBuf> {
    validate_id(id)?;
    Ok(homes_root()?.join(id))
}

/// Account ids: `ka-` + 1-32 of `[a-z0-9]`.
pub fn validate_id(id: &str) -> anyhow::Result<()> {
    let rest = id.strip_prefix(ID_PREFIX).unwrap_or("");
    ensure!(
        !rest.is_empty() && rest.len() <= 32 && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
        "invalid Kimi account id {id:?}"
    );
    Ok(())
}

fn random_suffix(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    bytes.iter().take(len).map(|byte| ALPHABET[(*byte & 31) as usize] as char).collect()
}

fn new_id() -> String {
    format!("{ID_PREFIX}{}", random_suffix(10))
}

pub fn validate_login_id(id: &str) -> bool {
    id.strip_prefix(LOGIN_ID_PREFIX)
        .is_some_and(|rest| rest.len() == 32 && rest.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()))
}

pub fn load() -> anyhow::Result<KimiAccountsFile> {
    let path = registry_path()?;
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(KimiAccountsFile::default()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let file: KimiAccountsFile =
        serde_json::from_slice(&raw).with_context(|| format!("parse Kimi accounts {}", path.display()))?;
    ensure!(file.version <= FILE_VERSION, "Kimi accounts file version {} is newer than this cutex", file.version);
    Ok(file)
}

/// Read-modify-write under the registry file lock.
pub fn update_registry<T>(change: impl FnOnce(&mut KimiAccountsFile) -> anyhow::Result<T>) -> anyhow::Result<T> {
    static IN_PROCESS: Mutex<()> = Mutex::new(());
    let _guard = IN_PROCESS.lock().unwrap_or_else(|e| e.into_inner());
    let lock = lock_path()?;
    if let Some(parent) = lock.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock)
        .with_context(|| format!("open {}", lock.display()))?;
    file.lock_exclusive().context("lock the Kimi accounts registry")?;
    let mut registry = load()?;
    let before = registry.clone();
    let result = change(&mut registry)?;
    if registry != before {
        registry.version = FILE_VERSION;
        write_private_pretty_json_atomic(&registry_path()?, &registry, "Kimi accounts")?;
    }
    drop(file);
    Ok(result)
}

pub fn validate_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    ensure!(
        !label.is_empty() && label.chars().count() <= MAX_LABEL_CHARS && !label.chars().any(char::is_control),
        "label must be 1-{MAX_LABEL_CHARS} visible characters"
    );
    Ok(label.to_string())
}

fn ensure_private_dir(dir: &Path) -> anyhow::Result<()> {
    crate::management::client_tokens::ensure_private_dir(dir)
}

pub fn add(label: &str, region: KimiRegion) -> anyhow::Result<KimiAccount> {
    let label = validate_label(label)?;
    let account = update_registry(|registry| {
        ensure!(registry.accounts.iter().all(|account| account.label != label), "a Kimi account labelled {label} already exists");
        let mut id = new_id();
        while registry.find(&id).is_some() {
            id = new_id();
        }
        let account = KimiAccount { id, label, region, created_at: chrono::Utc::now().to_rfc3339(), last_login_at: None };
        registry.accounts.push(account.clone());
        Ok(account)
    })?;
    ensure_private_dir(&homes_root()?)?;
    ensure_private_dir(&account_home(&account.id)?)?;
    Ok(account)
}

pub fn rename(id: &str, label: &str) -> anyhow::Result<KimiAccount> {
    let label = validate_label(label)?;
    update_registry(|registry| {
        ensure!(
            registry.accounts.iter().all(|account| account.id == id || account.label != label),
            "a Kimi account labelled {label} already exists"
        );
        let account = registry.accounts.iter_mut().find(|account| account.id == id).context("no such Kimi account")?;
        account.label = label;
        Ok(account.clone())
    })
}

// ---------------------------------------------------------------------------
// Usage and removal
// ---------------------------------------------------------------------------

pub use crate::runtime::claude_accounts::{ProfileRef, SessionRef};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub profiles: Vec<ProfileRef>,
    pub sessions: Vec<SessionRef>,
}

pub fn profiles_using(store: &AccountsStore, id: &str) -> Vec<ProfileRef> {
    store
        .accounts
        .iter()
        .filter(|profile| profile.cli_kind == CliKind::Kimi && profile.variants.kimi_account.as_deref() == Some(id))
        .map(|profile| ProfileRef { id: profile.id.clone(), name: profile.name.clone() })
        .collect()
}

/// Every account's usage: profiles naming it, sessions whose frozen binding is on it.
pub fn usage_by_account(registry: &KimiAccountsFile) -> BTreeMap<String, AccountUsage> {
    let mut out: BTreeMap<String, AccountUsage> =
        registry.accounts.iter().map(|account| (account.id.clone(), AccountUsage::default())).collect();
    if let Ok(store) = crate::profiles::settings_api::load_store() {
        for (id, usage) in out.iter_mut() {
            usage.profiles = profiles_using(&store, id);
        }
    }
    let _ = crate::session::store::with_cutex_session_store(|sessions| {
        for record in sessions.sessions.values() {
            let Some(account) = record.profile_binding.as_ref().and_then(|binding| binding.credential.kimi_account_id.as_deref()) else {
                continue;
            };
            let Some(usage) = out.get_mut(account) else { continue };
            usage.sessions.push(SessionRef {
                cutex_session_id: record.cutex_session_id.clone(),
                display_name: record
                    .formal_agent_name
                    .clone()
                    .or_else(|| record.thread_name.clone())
                    .unwrap_or_else(|| record.cutex_session_id.clone()),
                lifecycle: if record.is_retired() { "retired" } else { "active" },
                online: record.runtime_pid.is_some() && !record.is_retired(),
            });
        }
    });
    out
}

#[derive(Debug)]
pub enum RemoveError {
    NotFound,
    InUse(AccountUsage),
    Other(anyhow::Error),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such Kimi account"),
            Self::InUse(usage) => write!(
                f,
                "the Kimi account is still used by {} profile(s) and {} active session(s); rebind the profiles and archive the sessions first",
                usage.profiles.len(),
                usage.sessions.iter().filter(|s| s.lifecycle == "active").count()
            ),
            Self::Other(error) => write!(f, "{error:#}"),
        }
    }
}

impl From<anyhow::Error> for RemoveError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveOutcome {
    pub removed: KimiAccount,
    pub kept_home: Option<PathBuf>,
    pub home_deleted: bool,
}

pub fn remove(id: &str, delete_home: bool) -> Result<RemoveOutcome, RemoveError> {
    if validate_id(id).is_err() {
        return Err(RemoveError::NotFound);
    }
    let registry = load()?;
    if registry.find(id).is_none() {
        return Err(RemoveError::NotFound);
    }
    let usage = usage_by_account(&registry).remove(id).unwrap_or_default();
    if !usage.profiles.is_empty() || usage.sessions.iter().any(|s| s.lifecycle == "active") {
        return Err(RemoveError::InUse(usage));
    }
    cancel_logins_of(id);
    let removed = update_registry(|registry| {
        let index = registry.accounts.iter().position(|account| account.id == id).context("Kimi account removed concurrently")?;
        Ok(registry.accounts.remove(index))
    })?;
    let home = account_home(id)?;
    let mut home_deleted = false;
    if delete_home && home.exists() {
        std::fs::remove_dir_all(&home).with_context(|| format!("delete {}", home.display()))?;
        home_deleted = true;
    }
    Ok(RemoveOutcome { removed, kept_home: (!home_deleted && home.exists()).then_some(home), home_deleted })
}

// ---------------------------------------------------------------------------
// Credential state (presence / expiry / tombstone; never the tokens)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialState {
    Missing,
    /// A usable token bundle; `expires_at` is the access token's (unix s).
    Valid { expires_at: i64, scope: Option<String> },
    /// Kimi's revoked tombstone (refresh rejected, or cutex logout).
    Revoked,
}

impl CredentialState {
    pub fn usable(&self) -> bool {
        matches!(self, Self::Valid { .. })
    }
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Missing => "loggedOut",
            Self::Valid { .. } => "loggedIn",
            Self::Revoked => "revoked",
        }
    }
}

/// Validate a credential slot name the way Kimi's `FileTokenStorage` does.
pub fn validate_slot(slot: &str) -> anyhow::Result<()> {
    ensure!(
        !slot.is_empty()
            && slot.len() <= 128
            && !slot.starts_with('.')
            && slot.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "invalid Kimi credential slot"
    );
    Ok(())
}

/// The credential slot a Kimi home's `config.toml` names (`oauth/<slot>`).
pub fn slot_from_config(config: &toml::Table) -> Option<String> {
    let key = config
        .get("providers")?
        .get(super::kimi::MANAGED_PROVIDER)?
        .get("oauth")?
        .get("key")?
        .as_str()?;
    let slot = match key {
        "kimi-code" | "oauth/kimi-code" => DEFAULT_SLOT.to_string(),
        other => other.strip_prefix("oauth/").unwrap_or(other).to_string(),
    };
    validate_slot(&slot).ok().map(|()| slot)
}

/// The slot of an account home (its config, else the default).
pub fn home_slot(home: &Path) -> String {
    super::kimi::read_config(home).ok().and_then(|config| slot_from_config(&config)).unwrap_or_else(|| DEFAULT_SLOT.to_string())
}

pub fn credential_path(home: &Path, slot: &str) -> anyhow::Result<PathBuf> {
    validate_slot(slot)?;
    Ok(home.join("credentials").join(format!("{slot}.json")))
}

/// Classify a credential file as Kimi does (`token-state.ts`).
pub fn read_credential_state(path: &Path) -> CredentialState {
    let Ok(raw) = std::fs::read(path) else { return CredentialState::Missing };
    let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(&raw) else { return CredentialState::Missing };
    let non_empty = |key: &str| object.get(key).and_then(Value::as_str).is_some_and(|text| !text.is_empty());
    if !non_empty("access_token") {
        return CredentialState::Revoked;
    }
    if !non_empty("refresh_token") {
        // Kimi treats it as valid until a refresh is needed; it cannot be
        // refreshed, so it is not a usable login for cutex.
        return CredentialState::Revoked;
    }
    CredentialState::Valid {
        expires_at: object.get("expires_at").and_then(Value::as_i64).unwrap_or(0),
        scope: object.get("scope").and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty()),
    }
}

pub fn account_credential_state(id: &str) -> anyhow::Result<CredentialState> {
    let home = account_home(id)?;
    Ok(read_credential_state(&credential_path(&home, &home_slot(&home))?))
}

// ---------------------------------------------------------------------------
// Kimi's refresh lock (`proper-lockfile` compatible)
// ---------------------------------------------------------------------------

/// Holds `<home>/oauth/<slot>.lock` (a directory, like `proper-lockfile`):
/// every Kimi process of the account and cutex's own writes serialise on it.
/// `mkdir` is atomic on every platform (no `flock`). While held, its mtime is
/// renewed every second (as `proper-lockfile` does), so a holder that waits
/// on the OAuth host longer than [`LOCK_STALE`] is not taken for a crashed one.
pub struct RefreshLock {
    dir: PathBuf,
    keeper: Option<(Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>)>,
}

/// Sets a directory's mtime (Windows opens it with backup semantics).
pub fn touch_dir(path: &Path, time: SystemTime) -> std::io::Result<()> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(path)?;
    file.set_modified(time)
}

impl RefreshLock {
    pub fn acquire(home: &Path, slot: &str) -> anyhow::Result<Self> {
        Self::acquire_with(home, slot, LOCK_TIMEOUT)
    }

    pub fn acquire_with(home: &Path, slot: &str, timeout: Duration) -> anyhow::Result<Self> {
        validate_slot(slot)?;
        let oauth = home.join("oauth");
        std::fs::create_dir_all(&oauth).context("create the Kimi refresh lock directory")?;
        // proper-lockfile needs its target to exist (Kimi creates it the same way).
        OpenOptions::new().create(true).append(true).open(oauth.join(slot)).context("create the Kimi refresh lock target")?;
        let dir = oauth.join(format!("{slot}.lock"));
        let started = Instant::now();
        loop {
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let (flag, path) = (stop.clone(), dir.clone());
                    let keeper = std::thread::Builder::new()
                        .name("cutex-kimi-lock".into())
                        .spawn(move || {
                            let mut ticks = 0u32;
                            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(100));
                                ticks += 1;
                                if ticks % 10 == 0 {
                                    let _ = touch_dir(&path, SystemTime::now());
                                }
                            }
                        })
                        .ok()
                        .map(|handle| (stop, handle));
                    return Ok(Self { dir, keeper });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&dir)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_dir(&dir);
                        continue;
                    }
                }
                Err(error) => bail!("acquire the Kimi refresh lock: {}", error.kind()),
            }
            ensure!(started.elapsed() < timeout, "the Kimi refresh lock stayed busy for {}s", timeout.as_secs());
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        if let Some((stop, handle)) = self.keeper.take() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Kimi's revoked tombstone for a token file (keeps `scope` / `token_type`).
fn tombstone(previous: Option<&Value>) -> Value {
    let keep = |key: &str| previous.and_then(|value| value.get(key)).and_then(Value::as_str).unwrap_or("").to_string();
    json!({
        "access_token": "", "refresh_token": "", "expires_at": 0,
        "scope": keep("scope"), "token_type": keep("token_type"), "expires_in": 0,
    })
}

/// Logout: replace the credential with a tombstone under the refresh lock, so
/// no running Kimi process of the account can refresh it back (§3.2).
pub fn logout(id: &str) -> anyhow::Result<CredentialState> {
    let home = account_home(id)?;
    let slot = home_slot(&home);
    let path = credential_path(&home, &slot)?;
    let _lock = RefreshLock::acquire(&home, &slot)?;
    let previous = std::fs::read(&path).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
    if previous.is_some() || path.exists() {
        let data = serde_json::to_vec_pretty(&tombstone(previous.as_ref())).context("encode the Kimi logout tombstone")?;
        ensure_private_dir(&home.join("credentials"))?;
        write_private_bytes_atomic(&path, &data)?;
    }
    Ok(read_credential_state(&path))
}

/// Install a staged login into the account home under the refresh lock.
pub fn install_login(home: &Path, staging: &Path) -> anyhow::Result<()> {
    let config_text = std::fs::read_to_string(staging.join("config.toml")).context("kimi login wrote no config.toml")?;
    let config: toml::Table = toml::from_str(&config_text).map_err(|_| anyhow::anyhow!("kimi login wrote an invalid config.toml"))?;
    let slot = slot_from_config(&config).context("kimi login did not provision the managed Kimi Code provider")?;
    let staged = credential_path(staging, &slot)?;
    ensure!(read_credential_state(&staged).usable(), "kimi login stored no usable credential");
    let bytes = std::fs::read(&staged).context("read the staged Kimi credential")?;
    ensure_private_dir(home)?;
    ensure_private_dir(&home.join("credentials"))?;
    let _lock = RefreshLock::acquire(home, &slot)?;
    write_private_bytes_atomic(&credential_path(home, &slot)?, &bytes)?;
    write_private_bytes_atomic(&home.join("config.toml"), config_text.as_bytes())?;
    let device = staging.join("device_id");
    if device.is_file() && !home.join("device_id").exists() {
        write_private_bytes_atomic(&home.join("device_id"), &std::fs::read(&device)?)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Device-code login (drives the official `kimi login`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginState {
    Starting,
    Pending,
    Succeeded,
    Failed,
    Expired,
    Denied,
    Cancelled,
}

impl LoginState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Starting | Self::Pending)
    }
}

#[derive(Clone, Debug)]
pub struct LoginSnapshot {
    pub login_id: String,
    pub account_id: String,
    pub state: LoginState,
    pub verification_uri: Option<String>,
    pub user_code: Option<String>,
    pub expires_at: Option<String>,
    pub interval: u64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<(&'static str, String)>,
}

impl LoginSnapshot {
    pub fn to_wire(&self) -> Value {
        json!({
            "loginId": self.login_id, "accountId": self.account_id, "state": self.state.as_str(),
            "verificationUri": self.verification_uri, "userCode": self.user_code, "expiresAt": self.expires_at,
            "interval": self.interval, "startedAt": self.started_at, "finishedAt": self.finished_at,
            "error": self.error.as_ref().map(|(code, message)| json!({ "code": code, "message": message })),
        })
    }
}

struct LoginEntry {
    snapshot: LoginSnapshot,
    cancel: bool,
    /// The CLI succeeded and its credential is being installed: a cancel
    /// now waits for the outcome instead (the install is not undone).
    committing: bool,
    finished: Option<Instant>,
}

impl LoginEntry {
    /// The one way a login reaches a final state: never twice, so nothing
    /// (a late CLI line, its exit, the install) moves a cancelled login back.
    fn finish(&mut self, state: LoginState, error: Option<(&'static str, String)>) -> bool {
        if self.snapshot.state.is_final() {
            return false;
        }
        self.snapshot.state = state;
        self.snapshot.error = error;
        self.snapshot.finished_at = Some(chrono::Utc::now().to_rfc3339());
        self.finished = Some(Instant::now());
        true
    }
}

type Logins = Mutex<BTreeMap<String, Arc<(Mutex<LoginEntry>, Condvar)>>>;

fn logins() -> &'static Logins {
    static LOGINS: Logins = Mutex::new(BTreeMap::new());
    &LOGINS
}

fn forget_old_logins() {
    logins().lock().unwrap_or_else(|e| e.into_inner()).retain(|_, entry| {
        let entry = entry.0.lock().unwrap_or_else(|e| e.into_inner());
        entry.finished.is_none_or(|at| at.elapsed() < LOGIN_RETENTION)
    });
}

/// The account's current (pending) or last login of this daemon run.
pub fn login_of(account_id: &str) -> Option<LoginSnapshot> {
    forget_old_logins();
    let map = logins().lock().unwrap_or_else(|e| e.into_inner());
    map.get(account_id).map(|entry| entry.0.lock().unwrap_or_else(|e| e.into_inner()).snapshot.clone())
}

pub fn login_by_id(account_id: &str, login_id: &str) -> Option<LoginSnapshot> {
    login_of(account_id).filter(|login| login.login_id == login_id)
}

fn cancel_logins_of(account_id: &str) {
    if let Some(login) = login_of(account_id) {
        let _ = cancel_login(account_id, &login.login_id);
    }
}

pub fn cancel_login(account_id: &str, login_id: &str) -> Option<LoginSnapshot> {
    let entry = logins().lock().unwrap_or_else(|e| e.into_inner()).get(account_id).cloned()?;
    let (lock, signal) = &*entry;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if guard.snapshot.login_id != login_id {
        return None;
    }
    if !guard.snapshot.state.is_final() {
        guard.cancel = true;
        if guard.committing {
            // The CLI already succeeded: its install finishes (or fails) first.
            let deadline = Instant::now() + Duration::from_secs(10);
            while !guard.snapshot.state.is_final() && Instant::now() < deadline {
                guard = signal.wait_timeout(guard, Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
            }
        } else {
            // Cancelled at once (the fence): the driver stops the process and
            // never installs, and no later line or exit changes this state.
            guard.finish(LoginState::Cancelled, None);
            signal.notify_all();
        }
    }
    Some(guard.snapshot.clone())
}

#[derive(Debug)]
pub enum StartLoginError {
    NotFound,
    CliUnavailable(String),
    Other(anyhow::Error),
}

/// Start (or return the pending) device-code login of an account. Waits up to
/// `wait` for the device code. `Ok((snapshot, created))`.
pub fn start_login(account_id: &str, wait: Duration) -> Result<(LoginSnapshot, bool), StartLoginError> {
    let registry = load().map_err(StartLoginError::Other)?;
    let account = registry.find(account_id).cloned().ok_or(StartLoginError::NotFound)?;
    if let Some(existing) = login_of(account_id).filter(|login| !login.state.is_final()) {
        return Ok((existing, false));
    }
    let cli = super::kimi::cli().map_err(|error| StartLoginError::CliUnavailable(format!("{error:#}")))?;
    let login_id = format!("{LOGIN_ID_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let snapshot = LoginSnapshot {
        login_id: login_id.clone(),
        account_id: account_id.to_string(),
        state: LoginState::Starting,
        verification_uri: None,
        user_code: None,
        expires_at: None,
        interval: 5,
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
        error: None,
    };
    let entry = Arc::new((Mutex::new(LoginEntry { snapshot, cancel: false, committing: false, finished: None }), Condvar::new()));
    let staging = homes_root().and_then(|root| Ok(root.join(STAGING_DIR).join(&login_id))).map_err(StartLoginError::Other)?;
    let child = spawn_login(&cli, &account, &staging).map_err(|error| {
        let _ = std::fs::remove_dir_all(&staging);
        StartLoginError::Other(error)
    })?;
    logins().lock().unwrap_or_else(|e| e.into_inner()).insert(account_id.to_string(), entry.clone());
    let driver_entry = entry.clone();
    let account_id_owned = account_id.to_string();
    std::thread::Builder::new()
        .name(format!("kimi-login-{account_id}"))
        .spawn(move || drive_login(child, driver_entry, account_id_owned, staging))
        .map_err(|error| StartLoginError::Other(error.into()))?;
    let (lock, signal) = &*entry;
    let deadline = Instant::now() + wait;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    while guard.snapshot.state == LoginState::Starting && Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        guard = signal.wait_timeout(guard, left.min(Duration::from_millis(200))).unwrap_or_else(|e| e.into_inner()).0;
    }
    // Kimi prints the expiry right after the code: give it a moment.
    let settle = Instant::now() + Duration::from_millis(500);
    while guard.snapshot.state == LoginState::Pending && guard.snapshot.expires_at.is_none() && Instant::now() < settle {
        guard = signal.wait_timeout(guard, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
    }
    Ok((guard.snapshot.clone(), true))
}

fn shim_dir(staging: &Path) -> anyhow::Result<PathBuf> {
    let dir = staging.join("bin");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The daemon never opens a browser on the host: the desktop shows the URL.
        for name in ["xdg-open", "open", "sensible-browser", "www-browser"] {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\nexit 0\n")?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(dir)
}

fn spawn_login(cli: &super::kimi::KimiCli, account: &KimiAccount, staging: &Path) -> anyhow::Result<Child> {
    ensure_private_dir(&staging.parent().context("staging parent")?.to_path_buf())?;
    ensure_private_dir(staging)?;
    let home = account_home(&account.id)?;
    if let Ok(device) = std::fs::read(home.join("device_id")) {
        write_private_bytes_atomic(&staging.join("device_id"), &device)?;
    }
    let shims = shim_dir(staging)?;
    let tmp = staging.join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let mut path = std::ffi::OsString::from(shims.as_os_str());
    if let Some(host) = std::env::var_os("PATH") {
        path.push(if cfg!(windows) { ";" } else { ":" });
        path.push(host);
    }
    let mut command = Command::new(&cli.program);
    command.args(&cli.args).arg("login");
    let test_hosts = super::kimi::test_override(super::kimi::TEST_OAUTH_HOST_ENV)
        .zip(super::kimi::test_override(super::kimi::TEST_BASE_URL_ENV));
    if test_hosts.is_none() {
        command.arg("--region").arg(account.region.as_str());
    }
    command.env_clear();
    for key in ["SystemRoot", "WINDIR", "COMSPEC", "PATHEXT", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("PATH", path)
        .env("HOME", staging)
        .env("USERPROFILE", staging)
        .env("KIMI_CODE_HOME", staging)
        .env("XDG_CONFIG_HOME", staging.join(".config"))
        .env("XDG_CACHE_HOME", staging.join(".cache"))
        .env("XDG_DATA_HOME", staging.join(".local/share"))
        .env("XDG_STATE_HOME", staging.join(".local/state"))
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .env("KIMI_CODE_WATCH", "0")
        .env("KIMI_DISABLE_TELEMETRY", "1")
        .env("KIMI_DISABLE_CRON", "1")
        .env("KIMI_CODE_NO_AUTO_UPDATE", "1")
        .env("KIMI_CODE_REGION_MARKER", "off");
    for (key, value) in &cli.env {
        command.env(key, value);
    }
    if let Some((oauth_host, base_url)) = test_hosts {
        command.env("KIMI_CODE_OAUTH_HOST", oauth_host).env("KIMI_CODE_BASE_URL", base_url);
        if let Some(private) = std::env::var_os("CUTEX_TEST_PRIVATE_HOME") {
            command.env("CUTEX_TEST_PRIVATE_HOME", private);
        }
    }
    command.current_dir(staging).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map_err(|error| anyhow::anyhow!("start kimi login: {}", error.kind()))
}

fn kill_login(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// What a line of `kimi login` stderr tells cutex.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginLine {
    Url(String),
    Code(String),
    ExpiresIn(u64),
    Failure(String),
    Other,
}

fn url_allowed(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|parsed| {
        parsed.username().is_empty()
            && parsed.password().is_none()
            && (parsed.scheme() == "https"
                || (cfg!(debug_assertions)
                    && parsed.scheme() == "http"
                    && parsed.host_str().is_some_and(|host| host == "127.0.0.1" || host == "localhost")))
    })
}

pub fn parse_login_line(line: &str) -> LoginLine {
    let line = line.trim();
    if let Some(url) = line.strip_prefix("Opening browser for Kimi device login: ") {
        let url = url.trim();
        return if url_allowed(url) && url.len() <= 1024 { LoginLine::Url(url.to_string()) } else { LoginLine::Other };
    }
    if let Some((_, code)) = line.split_once("enter code: ") {
        let code = code.trim();
        if !code.is_empty() && code.len() <= 64 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return LoginLine::Code(code.to_string());
        }
        return LoginLine::Other;
    }
    if let Some(rest) = line.strip_prefix("Code expires in ") {
        if let Some(seconds) = rest.strip_suffix("s.").and_then(|n| n.parse::<u64>().ok()) {
            return LoginLine::ExpiresIn(seconds);
        }
    }
    for prefix in ["Login failed: ", "Login cancelled: "] {
        if let Some(message) = line.strip_prefix(prefix) {
            return LoginLine::Failure(message.chars().take(300).collect());
        }
    }
    LoginLine::Other
}

fn classify_failure(message: &str) -> (LoginState, &'static str) {
    let lower = message.to_ascii_lowercase();
    if lower.contains("denied") {
        (LoginState::Denied, "kimi_login_denied")
    } else if lower.contains("timed out") || lower.contains("expired") {
        (LoginState::Expired, "kimi_login_timeout")
    } else {
        (LoginState::Failed, "kimi_login_failed")
    }
}

fn drive_login(mut child: Child, entry: Arc<(Mutex<LoginEntry>, Condvar)>, account_id: String, staging: PathBuf) {
    let (lock, signal) = &*entry;
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut reader = child.stderr.take().map(|stderr| {
        let entry = entry.clone();
        let failure = failure.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.by_ref().take(MAX_STDERR_LINE as u64).read_until(b'\n', &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buffer).to_string();
                let (lock, signal) = &*entry;
                let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                if guard.snapshot.state.is_final() {
                    // Cancelled: late lines change nothing.
                    continue;
                }
                match parse_login_line(&line) {
                    LoginLine::Url(url) => guard.snapshot.verification_uri = Some(url),
                    LoginLine::Code(code) => guard.snapshot.user_code = Some(code),
                    LoginLine::ExpiresIn(seconds) => {
                        guard.snapshot.expires_at =
                            Some((chrono::Utc::now() + chrono::Duration::seconds(seconds.min(86_400) as i64)).to_rfc3339())
                    }
                    LoginLine::Failure(message) => *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(message),
                    LoginLine::Other => {}
                }
                if guard.snapshot.state == LoginState::Starting
                    && guard.snapshot.verification_uri.is_some()
                    && guard.snapshot.user_code.is_some()
                {
                    guard.snapshot.state = LoginState::Pending;
                }
                signal.notify_all();
            }
        })
    });
    let started = Instant::now();
    let outcome: (LoginState, Option<(&'static str, String)>) = loop {
        let cancel = lock.lock().unwrap_or_else(|e| e.into_inner()).cancel;
        if cancel {
            kill_login(&mut child);
            break (LoginState::Cancelled, None);
        }
        if started.elapsed() > LOGIN_DEADLINE {
            kill_login(&mut child);
            break (LoginState::Expired, Some(("kimi_login_timeout", "the device code was not confirmed in time".into())));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(reader) = reader.take() {
                    let _ = reader.join();
                }
                if status.success() {
                    {
                        // The fence: a cancel that came first wins; otherwise
                        // from here on a cancel waits for the install.
                        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if guard.cancel {
                            break (LoginState::Cancelled, None);
                        }
                        guard.committing = true;
                    }
                    let home = account_home(&account_id);
                    match home.and_then(|home| install_login(&home, &staging)) {
                        Ok(()) => {
                            let _ = update_registry(|registry| {
                                if let Some(account) = registry.accounts.iter_mut().find(|account| account.id == account_id) {
                                    account.last_login_at = Some(chrono::Utc::now().to_rfc3339());
                                }
                                Ok(())
                            });
                            break (LoginState::Succeeded, None);
                        }
                        Err(error) => break (LoginState::Failed, Some(("kimi_login_failed", format!("{error:#}")))),
                    }
                }
                let message = failure.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| format!("kimi login exited {status}"));
                let (state, code) = classify_failure(&message);
                break (state, Some((code, message)));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                kill_login(&mut child);
                break (LoginState::Failed, Some(("kimi_login_failed", format!("wait for kimi login: {}", error.kind()))));
            }
        }
    };
    let _ = std::fs::remove_dir_all(&staging);
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    // A login cancelled meanwhile stays cancelled (`finish` is first-wins).
    guard.finish(outcome.0, outcome.1);
    signal.notify_all();
}

// ---------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------

fn status_json(state: &CredentialState) -> Value {
    let (expires_at, scope) = match state {
        CredentialState::Valid { expires_at, scope } => (
            (*expires_at > 0)
                .then(|| chrono::DateTime::<chrono::Utc>::from_timestamp(*expires_at, 0).map(|at| at.to_rfc3339()))
                .flatten(),
            scope.clone(),
        ),
        _ => (None, None),
    };
    json!({
        "state": state.state_name(), "loggedIn": state.usable(), "expiresAt": expires_at, "scope": scope,
        "checkedAt": chrono::Utc::now().to_rfc3339(),
    })
}

pub fn item_json(registry: &KimiAccountsFile, id: &str, usage: Option<&AccountUsage>) -> Option<Value> {
    let account = registry.find(id)?;
    let empty = AccountUsage::default();
    let usage = usage.unwrap_or(&empty);
    let home = account_home(&account.id).ok();
    let state = account_credential_state(&account.id).unwrap_or(CredentialState::Missing);
    let catalog = home
        .as_deref()
        .and_then(|home| super::kimi::read_config(home).ok())
        .map(|config| super::kimi::catalog_from_config(&config))
        .unwrap_or_default();
    Some(json!({
        "id": account.id,
        "label": account.label,
        "kind": "subscription",
        "region": account.region.as_str(),
        "home": home,
        "createdAt": account.created_at,
        "lastLoginAt": account.last_login_at,
        "credentialsPresent": state.usable(),
        "status": status_json(&state),
        "login": login_of(&account.id).map(|login| login.to_wire()),
        "models": catalog.wire(),
        "profiles": usage.profiles,
        "sessions": {
            "active": usage.sessions.iter().filter(|s| s.lifecycle == "active").count(),
            "online": usage.sessions.iter().filter(|s| s.online).count(),
            "retired": usage.sessions.iter().filter(|s| s.lifecycle == "retired").count(),
        },
        "removable": usage.profiles.is_empty() && !usage.sessions.iter().any(|s| s.lifecycle == "active"),
    }))
}

pub fn list_json() -> anyhow::Result<Value> {
    let registry = load()?;
    let usage = usage_by_account(&registry);
    let items: Vec<Value> =
        registry.accounts.iter().filter_map(|account| item_json(&registry, &account.id, usage.get(&account.id))).collect();
    Ok(json!({ "contractVersion": 2, "accounts": items }))
}

#[cfg(test)]
#[path = "kimi_accounts_tests.rs"]
pub(crate) mod tests;
