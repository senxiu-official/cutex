//! Claude accounts: several official Claude Code logins (or API keys) on one
//! host (cutex-desktop docs/research/claude-multi-account.md).
//!
//! An account is a Claude Code config directory (`CLAUDE_CONFIG_DIR`):
//!
//! - `default`: implicit, not stored, never removable. Its home is the
//!   selected deployment's `claude_home` (the shared login every Claude
//!   session used before accounts existed). Without any added account nothing
//!   changes.
//! - `subscription`: `<config dir>/claude-accounts/<id>/`, logged in by the
//!   owner in a terminal (`cutex human claude account login <id>`, i.e. the
//!   official `claude auth login` with that `CLAUDE_CONFIG_DIR`). cutex never
//!   reads or copies its credentials (the quota reader is the one existing
//!   exception, as for the default home).
//! - `api`: the same kind of home plus an API key (owner-private
//!   `<config dir>/claude-account-keys/<id>.json`, like the Codex API-key
//!   profiles' `auth.json`) and an optional Anthropic-compatible base URL.
//!   Only the adapter of a session on that account reads the key, and only
//!   that session's Claude Code process gets it (`apiKeyHelper`, claude-r12+;
//!   older adapters put it in that process's env as `ANTHROPIC_API_KEY` or
//!   `ANTHROPIC_AUTH_TOKEN`); it needs an adapter that declares
//!   `features.apiAccounts` (claude-r7+).
//!
//! The registry is `<runtime>/claude/accounts.json` (owner-private, file
//! locked for read-modify-write). A profile names its account in
//! `StoredAccount.claude_account` (absent = `default`). A session freezes the
//! account's home into its `ClaudeSession.deployment.claude_home` at creation
//! (the session record schema is unchanged); the account of a session is
//! found again from that home. A session never changes account: a profile
//! change that would resolve to another account is refused
//! ([`check_profile_change`], error code `claude_account_mismatch`).
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::profiles::model::{AccountsStore, CliKind};
use crate::session::model::CutexSessionRecord;

pub const DEFAULT_ACCOUNT_ID: &str = "default";
pub const MAX_LABEL_CHARS: usize = 64;
pub const MAX_API_KEY_BYTES: usize = 4096;
const FILE_VERSION: u32 = 1;
const ID_PREFIX: &str = "ca-";
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STATUS_FIELD_CHARS: usize = 256;
/// Directory (under the cutex config dir) holding the account homes.
pub const HOMES_DIR: &str = "claude-accounts";
/// Directory (under the cutex config dir) holding the API keys.
pub const KEYS_DIR: &str = "claude-account-keys";
/// Per-session file the launcher writes for an API-account adapter.
pub const API_ACCOUNT_CONFIG_FILE: &str = "claude-api-account.json";

/// Environment the official runtime must not inherit from the daemon: it
/// would route a subscription session to an API key / provider. Mirrors the
/// adapter's `CONFLICTING_ENV`.
pub const CONFLICTING_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_MANTLE",
];

/// Non-secret Claude Code settings an API account may set for its sessions
/// (model aliases for an Anthropic-compatible provider, traffic / caching
/// switches). Anything else is refused.
pub const API_ENV_ALLOWED: &[&str] = &[
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
    "DISABLE_TELEMETRY",
    "DISABLE_ERROR_REPORTING",
    "DISABLE_PROMPT_CACHING",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "MAX_THINKING_TOKENS",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeAccountKind {
    /// Official claude.ai login (Pro / Max / Team / Enterprise seat).
    Subscription,
    /// API key (Anthropic or an Anthropic-compatible endpoint).
    Api,
}

impl ClaudeAccountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Api => "api",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "subscription" => Some(Self::Subscription),
            "api" => Some(Self::Api),
            _ => None,
        }
    }
}

/// How the key reaches Claude Code: `ANTHROPIC_API_KEY` (`x-api-key`
/// header; `claude auth status` reports it) or `ANTHROPIC_AUTH_TOKEN`
/// (`Authorization: Bearer`, for providers that want it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiKeyEnv {
    #[default]
    #[serde(rename = "ANTHROPIC_API_KEY")]
    ApiKey,
    #[serde(rename = "ANTHROPIC_AUTH_TOKEN")]
    AuthToken,
}

impl ApiKeyEnv {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "ANTHROPIC_API_KEY",
            Self::AuthToken => "ANTHROPIC_AUTH_TOKEN",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ANTHROPIC_API_KEY" | "apiKey" | "api-key" | "x-api-key" => Some(Self::ApiKey),
            "ANTHROPIC_AUTH_TOKEN" | "authToken" | "auth-token" | "bearer" => Some(Self::AuthToken),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeApiSettings {
    /// `ANTHROPIC_BASE_URL`; absent = Anthropic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub key_env: ApiKeyEnv,
    /// Extra non-secret settings ([`API_ENV_ALLOWED`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl ClaudeApiSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(url) = &self.base_url {
            validate_base_url(url)?;
        }
        ensure!(self.env.len() <= API_ENV_ALLOWED.len(), "too many env entries");
        for (key, value) in &self.env {
            ensure!(
                API_ENV_ALLOWED.contains(&key.as_str()),
                "env {key} is not allowed for a Claude API account (allowed: {})",
                API_ENV_ALLOWED.join(", ")
            );
            ensure!(
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
                "env {key} must be 1-256 visible characters"
            );
        }
        Ok(())
    }
}

pub fn validate_base_url(text: &str) -> anyhow::Result<()> {
    let url = url::Url::parse(text.trim()).map_err(|error| anyhow::anyhow!("invalid base URL: {error}"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "base URL must be an http(s) URL with a host"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "base URL must not embed credentials"
    );
    ensure!(url.query().is_none() && url.fragment().is_none(), "base URL must not carry a query or fragment");
    Ok(())
}

/// Non-secret fields of `claude auth status` (subscription accounts) or
/// of the stored key (API accounts). Never a token.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeAccountStatus {
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub api_provider: Option<String>,
    pub api_key_source: Option<String>,
    pub email: Option<String>,
    pub org_id: Option<String>,
    pub org_name: Option<String>,
    pub subscription_type: Option<String>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccount {
    pub id: String,
    pub label: String,
    pub kind: ClaudeAccountKind,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ClaudeApiSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<ClaudeAccountStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccountsFile {
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<ClaudeAccount>,
    /// Last status check of the implicit `default` account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_status: Option<ClaudeAccountStatus>,
}

impl Default for ClaudeAccountsFile {
    fn default() -> Self {
        Self { version: FILE_VERSION, accounts: Vec::new(), default_status: None }
    }
}

impl ClaudeAccountsFile {
    pub fn find(&self, id: &str) -> Option<&ClaudeAccount> {
        self.accounts.iter().find(|account| account.id == id)
    }
}

/// A profile change that would move a Claude session to another account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAccountMismatch {
    pub session_account: String,
    pub target_account: String,
    pub profile: Option<String>,
}

impl std::fmt::Display for ClaudeAccountMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this Claude agent is bound to Claude account {}; profile {} uses account {} (create a new agent to use it)",
            self.session_account,
            self.profile.as_deref().unwrap_or("(none)"),
            self.target_account
        )
    }
}

impl std::error::Error for ClaudeAccountMismatch {}

pub const MISMATCH_CODE: &str = "claude_account_mismatch";

// ---------------------------------------------------------------------------
// Paths and persistence
// ---------------------------------------------------------------------------

pub fn registry_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("claude/accounts.json"))
}

fn lock_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::runtime_dir()?.join("claude/accounts.lock"))
}

pub fn homes_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(HOMES_DIR))
}

fn keys_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(KEYS_DIR))
}

/// `CLAUDE_CONFIG_DIR` of an added account (not `default`).
pub fn account_home(id: &str) -> anyhow::Result<PathBuf> {
    validate_id(id)?;
    Ok(homes_root()?.join(id))
}

/// The API key file of an API account.
pub fn key_path(id: &str) -> anyhow::Result<PathBuf> {
    validate_id(id)?;
    Ok(keys_root()?.join(format!("{id}.json")))
}

/// Added account ids: `ca-` + 1-32 of `[a-z0-9]`.
pub fn validate_id(id: &str) -> anyhow::Result<()> {
    let rest = id.strip_prefix(ID_PREFIX).unwrap_or("");
    ensure!(
        !rest.is_empty() && rest.len() <= 32 && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
        "invalid Claude account id {id:?}"
    );
    Ok(())
}

fn new_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut out = String::from(ID_PREFIX);
    for byte in bytes.iter().take(10) {
        out.push(ALPHABET[(*byte & 31) as usize] as char);
    }
    out
}

pub fn load() -> anyhow::Result<ClaudeAccountsFile> {
    let path = registry_path()?;
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ClaudeAccountsFile::default()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let file: ClaudeAccountsFile =
        serde_json::from_slice(&raw).with_context(|| format!("parse Claude accounts {}", path.display()))?;
    ensure!(
        file.version <= FILE_VERSION,
        "Claude accounts file version {} is newer than this cutex",
        file.version
    );
    Ok(file)
}

/// Cached by modification time: session resources resolve every Claude
/// session's account from it.
pub fn load_cached() -> ClaudeAccountsFile {
    type Cache = Option<(Option<std::time::SystemTime>, ClaudeAccountsFile)>;
    static CACHE: std::sync::Mutex<Cache> = std::sync::Mutex::new(None);
    let modified = registry_path()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, file)) = cache.as_ref() {
        if *at == modified && modified.is_some() {
            return file.clone();
        }
    }
    let file = load().unwrap_or_default();
    *cache = Some((modified, file.clone()));
    file
}

/// Read-modify-write under the registry file lock.
pub fn update_registry<T>(change: impl FnOnce(&mut ClaudeAccountsFile) -> anyhow::Result<T>) -> anyhow::Result<T> {
    static IN_PROCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
    file.lock_exclusive().context("lock the Claude accounts registry")?;
    let mut registry = load()?;
    let before = registry.clone();
    let result = change(&mut registry)?;
    if registry != before {
        registry.version = FILE_VERSION;
        write_private_pretty_json_atomic(&registry_path()?, &registry, "Claude accounts")?;
    }
    drop(file);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Add / update / remove
// ---------------------------------------------------------------------------

pub fn validate_label(label: &str) -> anyhow::Result<String> {
    let label = label.trim();
    ensure!(
        !label.is_empty() && label.chars().count() <= MAX_LABEL_CHARS && !label.chars().any(char::is_control),
        "label must be 1-{MAX_LABEL_CHARS} visible characters"
    );
    ensure!(
        !label.eq_ignore_ascii_case(DEFAULT_ACCOUNT_ID),
        "\"default\" names the shared account"
    );
    Ok(label.to_string())
}

pub fn validate_api_key(key: &str) -> anyhow::Result<String> {
    let key = key.trim();
    ensure!(
        !key.is_empty() && key.len() <= MAX_API_KEY_BYTES && !key.chars().any(|c| c.is_control() || c.is_whitespace()),
        "API key must be 1-{MAX_API_KEY_BYTES} non-whitespace characters"
    );
    Ok(key.to_string())
}

pub struct NewClaudeAccount {
    pub label: String,
    pub kind: ClaudeAccountKind,
    pub api: Option<ClaudeApiSettings>,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for NewClaudeAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewClaudeAccount")
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("api", &self.api)
            .field("apiKey", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

fn write_key(id: &str, key: &str) -> anyhow::Result<()> {
    let root = keys_root()?;
    crate::management::client_tokens::ensure_private_dir(&root)?;
    write_private_pretty_json_atomic(&key_path(id)?, &json!({ "apiKey": key }), "Claude account API key")
}

pub fn api_key_present(id: &str) -> bool {
    key_path(id)
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .is_some_and(|metadata| metadata.len() > 0)
}

/// Create an account: its private home, the key file (API) and the
/// registry entry. A failure leaves nothing behind.
pub fn add(request: NewClaudeAccount) -> anyhow::Result<ClaudeAccount> {
    let label = validate_label(&request.label)?;
    let api = match request.kind {
        ClaudeAccountKind::Subscription => {
            ensure!(
                request.api.is_none() && request.api_key.is_none(),
                "baseUrl, keyEnv, env and apiKey apply to API accounts only"
            );
            None
        }
        ClaudeAccountKind::Api => {
            let api = request.api.unwrap_or_default();
            api.validate()?;
            Some(api)
        }
    };
    let api_key = request.api_key.as_deref().map(validate_api_key).transpose()?;
    update_registry(|registry| {
        ensure!(
            !registry.accounts.iter().any(|account| account.label == label),
            "a Claude account labelled {label} already exists"
        );
        let id = loop {
            let id = new_id();
            if registry.find(&id).is_none() && !account_home(&id)?.exists() {
                break id;
            }
        };
        let home = account_home(&id)?;
        let created = (|| -> anyhow::Result<()> {
            crate::management::client_tokens::ensure_private_dir(&homes_root()?)?;
            crate::management::client_tokens::ensure_private_dir(&home)?;
            if let Some(key) = &api_key {
                write_key(&id, key)?;
            }
            Ok(())
        })();
        if let Err(error) = created {
            let _ = std::fs::remove_dir_all(&home);
            let _ = std::fs::remove_file(key_path(&id)?);
            return Err(error);
        }
        let account = ClaudeAccount {
            id,
            label: label.clone(),
            kind: request.kind,
            created_at: chrono::Utc::now().to_rfc3339(),
            api,
            last_status: None,
        };
        registry.accounts.push(account.clone());
        Ok(account)
    })
}

/// Tri-state field of an update: absent / clear / set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Change<T> {
    #[default]
    Unchanged,
    Clear,
    Set(T),
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct AccountPatch {
    pub label: Option<String>,
    pub base_url: Change<String>,
    pub key_env: Option<ApiKeyEnv>,
    /// Replaces the whole map.
    pub env: Option<BTreeMap<String, String>>,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for AccountPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountPatch")
            .field("label", &self.label)
            .field("base_url", &self.base_url)
            .field("key_env", &self.key_env)
            .field("env", &self.env)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl AccountPatch {
    fn touches_api(&self) -> bool {
        self.base_url != Change::Unchanged || self.key_env.is_some() || self.env.is_some() || self.api_key.is_some()
    }
}

/// Change an added account (label; API settings and key of an API account).
/// API settings apply when a session's adapter starts next.
pub fn update(id: &str, patch: AccountPatch) -> anyhow::Result<ClaudeAccount> {
    ensure!(id != DEFAULT_ACCOUNT_ID, "the default account has no editable settings");
    validate_id(id)?;
    let label = patch.label.as_deref().map(validate_label).transpose()?;
    let api_key = patch.api_key.as_deref().map(validate_api_key).transpose()?;
    update_registry(|registry| {
        if let Some(label) = &label {
            ensure!(
                !registry.accounts.iter().any(|account| account.id != id && &account.label == label),
                "a Claude account labelled {label} already exists"
            );
        }
        let account = registry
            .accounts
            .iter_mut()
            .find(|account| account.id == id)
            .with_context(|| format!("no Claude account {id}"))?;
        if patch.touches_api() {
            ensure!(account.kind == ClaudeAccountKind::Api, "baseUrl, keyEnv, env and apiKey apply to API accounts only");
            let mut api = account.api.clone().unwrap_or_default();
            match &patch.base_url {
                Change::Unchanged => {}
                Change::Clear => api.base_url = None,
                Change::Set(url) => api.base_url = Some(url.trim().to_string()),
            }
            if let Some(key_env) = patch.key_env {
                api.key_env = key_env;
            }
            if let Some(env) = &patch.env {
                api.env = env.clone();
            }
            api.validate()?;
            account.api = Some(api);
        }
        if let Some(label) = label {
            account.label = label;
        }
        if let Some(key) = &api_key {
            write_key(id, key)?;
        }
        Ok(account.clone())
    })
}

/// What references an account: profiles bound to it and sessions created
/// on it (their frozen home is the account home).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub profiles: Vec<ProfileRef>,
    pub sessions: Vec<SessionRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRef {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRef {
    pub cutex_session_id: String,
    pub display_name: String,
    /// `active` | `retired`.
    pub lifecycle: &'static str,
    pub online: bool,
}

/// Profiles bound to `id` (`default`: Claude profiles without an account).
pub fn profiles_using(store: &AccountsStore, id: &str) -> Vec<ProfileRef> {
    store
        .accounts
        .iter()
        .filter(|profile| profile.cli_kind == CliKind::Claude)
        .filter(|profile| profile.claude_account.as_deref().unwrap_or(DEFAULT_ACCOUNT_ID) == id)
        .map(|profile| ProfileRef { id: profile.id.clone(), name: profile.name.clone() })
        .collect()
}

/// Every account's usage in one pass over the profile and session stores.
pub fn usage_by_account(registry: &ClaudeAccountsFile) -> BTreeMap<String, AccountUsage> {
    let mut out: BTreeMap<String, AccountUsage> = BTreeMap::new();
    out.insert(DEFAULT_ACCOUNT_ID.to_string(), AccountUsage::default());
    for account in &registry.accounts {
        out.insert(account.id.clone(), AccountUsage::default());
    }
    if let Ok(store) = crate::profiles::settings_api::load_store() {
        for (id, usage) in out.iter_mut() {
            usage.profiles = profiles_using(&store, id);
        }
    }
    let _ = crate::session::store::with_cutex_session_store(|sessions| {
        for record in sessions.sessions.values() {
            let Some(session) = &record.claude_session else {
                continue;
            };
            let account = account_id_for_home(registry, &session.deployment.claude_home);
            let Some(usage) = out.get_mut(&account) else {
                continue;
            };
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
    IsDefault,
    NotFound,
    InUse(AccountUsage),
    Other(anyhow::Error),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IsDefault => f.write_str("the default Claude account cannot be removed"),
            Self::NotFound => f.write_str("no such Claude account"),
            Self::InUse(usage) => write!(
                f,
                "the Claude account is still used by {} profile(s) and {} active session(s); rebind the profiles and archive the sessions first",
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
    pub removed: ClaudeAccount,
    /// The home (native transcripts, login) when it was kept.
    pub kept_home: Option<PathBuf>,
    pub home_deleted: bool,
}

/// Deregister an account and delete its API key. Refused while a profile is
/// bound to it or an active (not archived) session runs on it. The home
/// (login, native Claude history) is kept unless `delete_home`: archived
/// sessions on it can still start after a restore while it exists.
pub fn remove(id: &str, delete_home: bool) -> Result<RemoveOutcome, RemoveError> {
    if id == DEFAULT_ACCOUNT_ID {
        return Err(RemoveError::IsDefault);
    }
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
    let removed = update_registry(|registry| {
        let index = registry
            .accounts
            .iter()
            .position(|account| account.id == id)
            .context("Claude account removed concurrently")?;
        Ok(registry.accounts.remove(index))
    })?;
    match std::fs::remove_file(key_path(id)?) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RemoveError::Other(anyhow::Error::from(error).context("remove the API key"))),
    }
    let home = account_home(id)?;
    let mut home_deleted = false;
    if delete_home && home.exists() {
        std::fs::remove_dir_all(&home).with_context(|| format!("delete {}", home.display()))?;
        home_deleted = true;
    }
    Ok(RemoveOutcome {
        removed,
        kept_home: (!home_deleted && home.exists()).then_some(home),
        home_deleted,
    })
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
        norm(a) == norm(b)
    } else {
        a == b || a.to_string_lossy().trim_end_matches('/') == b.to_string_lossy().trim_end_matches('/')
    }
}

/// The account a frozen session home belongs to: a registered account, a
/// removed one (`removed:<id>`, a home under the accounts dir no longer
/// registered; it matches nothing else), or `default` (any other home: the
/// deployment homes every session used before accounts existed).
pub fn account_id_for_home(registry: &ClaudeAccountsFile, home: &Path) -> String {
    if let Ok(root) = homes_root() {
        for account in &registry.accounts {
            if same_path(&root.join(&account.id), home) {
                return account.id.clone();
            }
        }
        if let Some(parent) = home.parent() {
            if same_path(parent, &root) {
                let name = home.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return format!("removed:{name}");
            }
        }
    }
    DEFAULT_ACCOUNT_ID.to_string()
}

/// Account of a Claude session (None for other sessions).
pub fn session_account_id(registry: &ClaudeAccountsFile, record: &CutexSessionRecord) -> Option<String> {
    record
        .claude_session
        .as_ref()
        .map(|session| account_id_for_home(registry, &session.deployment.claude_home))
}

/// The account a profile name resolves to: its `claude_account`, else
/// `default`. Unknown / non-Claude profiles resolve to None.
pub fn profile_account_id(store: &AccountsStore, profile: Option<&str>) -> Option<String> {
    let Some(name) = profile.map(str::trim).filter(|name| !name.is_empty()) else {
        return Some(DEFAULT_ACCOUNT_ID.to_string());
    };
    let account = crate::profiles::settings_api::find(store, name)?;
    (account.cli_kind == CliKind::Claude)
        .then(|| account.claude_account.clone().unwrap_or_else(|| DEFAULT_ACCOUNT_ID.to_string()))
}

/// Refuse a profile change of a Claude session that would resolve to
/// another Claude account. Non-Claude sessions and profiles that are not
/// Claude profiles (other checks refuse those) pass.
pub fn check_profile_change_with(
    registry: &ClaudeAccountsFile,
    store: &AccountsStore,
    record: &CutexSessionRecord,
    profile: Option<&str>,
) -> Result<(), ClaudeAccountMismatch> {
    let Some(session_account) = session_account_id(registry, record) else {
        return Ok(());
    };
    let Some(target_account) = profile_account_id(store, profile) else {
        return Ok(());
    };
    if session_account == target_account {
        return Ok(());
    }
    Err(ClaudeAccountMismatch {
        session_account,
        target_account,
        profile: profile.map(str::to_string),
    })
}

/// [`check_profile_change_with`] on the current registry and profile store.
/// A store read failure does not block: the change is then judged by the
/// account registry only (an unreadable profile store fails elsewhere).
pub fn check_profile_change(record: &CutexSessionRecord, profile: Option<&str>) -> Result<(), ClaudeAccountMismatch> {
    if record.claude_session.is_none() {
        return Ok(());
    }
    let Ok(store) = crate::profiles::settings_api::load_store() else {
        return Ok(());
    };
    check_profile_change_with(&load_cached(), &store, record, profile)
}

/// The home a new session on `profile` freezes: the profile's account home,
/// or `default_home` (the selected deployment's) for `default`.
pub fn home_for_profile(profile: Option<&str>, default_home: &Path) -> anyhow::Result<(String, PathBuf)> {
    if profile.map(str::trim).is_none_or(str::is_empty) {
        return Ok((DEFAULT_ACCOUNT_ID.to_string(), default_home.to_path_buf()));
    }
    let store = crate::profiles::settings_api::load_store()?;
    let account = profile_account_id(&store, profile).unwrap_or_else(|| DEFAULT_ACCOUNT_ID.to_string());
    if account == DEFAULT_ACCOUNT_ID {
        return Ok((account, default_home.to_path_buf()));
    }
    let registry = load()?;
    ensure!(
        registry.find(&account).is_some(),
        "profile {} uses Claude account {account}, which is not registered on this host (cutex human claude account list)",
        profile.unwrap_or_default()
    );
    let home = account_home(&account)?;
    ensure!(home.is_dir(), "Claude account {account} home is missing: {}", home.display());
    Ok((account, home))
}

/// Home of an account id (`default` = the selected deployment's).
pub fn home_for(id: &str) -> anyhow::Result<PathBuf> {
    if id == DEFAULT_ACCOUNT_ID {
        return Ok(crate::runtime::claude::ClaudeDeployment::selected()?.claude_home);
    }
    let registry = load()?;
    ensure!(registry.find(id).is_some(), "no Claude account {id}");
    account_home(id)
}

/// Settings the launcher hands to the adapter of a session whose home is
/// an API account (None for subscription / default homes).
pub fn api_launch_config(home: &Path) -> anyhow::Result<Option<Value>> {
    // Deployment homes (every session before accounts existed) never read
    // the registry: its state cannot affect them.
    let root = homes_root()?;
    if !home.parent().is_some_and(|parent| same_path(parent, &root)) {
        return Ok(None);
    }
    let registry = load()?;
    let id = account_id_for_home(&registry, home);
    if let Some(removed) = id.strip_prefix("removed:") {
        // A deregistered subscription home keeps working while it exists;
        // an API account without its registration has no key.
        if std::path::Path::new(&crate::config::paths::config_dir()?.join(KEYS_DIR).join(format!("{removed}.json"))).exists() {
            bail!("Claude account {removed} is no longer registered");
        }
        return Ok(None);
    }
    let Some(account) = registry.find(&id) else {
        return Ok(None);
    };
    if account.kind != ClaudeAccountKind::Api {
        return Ok(None);
    }
    ensure!(
        api_key_present(&account.id),
        "Claude API account {} ({}) has no API key; set one (cutex human claude account set {} --api-key-stdin)",
        account.label,
        account.id,
        account.id
    );
    let api = account.api.clone().unwrap_or_default();
    api.validate()?;
    Ok(Some(json!({
        "version": 1,
        "accountId": account.id,
        "keyFile": key_path(&account.id)?,
        "keyEnv": api.key_env.as_str(),
        "baseUrl": api.base_url,
        "env": api.env,
    })))
}

// ---------------------------------------------------------------------------
// Status, login, logout (official CLI)
// ---------------------------------------------------------------------------

fn official_command(cli: &Path, home: &Path) -> Command {
    let mut command = Command::new(cli);
    for key in CONFLICTING_ENV {
        command.env_remove(key);
    }
    command.env("CLAUDE_CONFIG_DIR", home);
    command
}

fn bounded_field(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(|text| text.chars().filter(|c| !c.is_control()).take(MAX_STATUS_FIELD_CHARS).collect::<String>())
        .filter(|text| !text.is_empty())
}

/// Non-secret fields of a `claude auth status` JSON payload.
pub fn parse_auth_status(payload: &Value) -> ClaudeAccountStatus {
    ClaudeAccountStatus {
        logged_in: payload.get("loggedIn").and_then(Value::as_bool).unwrap_or(false),
        auth_method: bounded_field(payload, "authMethod"),
        api_provider: bounded_field(payload, "apiProvider"),
        api_key_source: bounded_field(payload, "apiKeySource"),
        email: bounded_field(payload, "email"),
        org_id: bounded_field(payload, "orgId"),
        org_name: bounded_field(payload, "orgName"),
        subscription_type: bounded_field(payload, "subscriptionType"),
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        error: None,
    }
}

fn run_auth_status(cli: &Path, home: &Path) -> anyhow::Result<ClaudeAccountStatus> {
    let mut command = official_command(cli, home);
    command
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().with_context(|| format!("run {} auth status", cli.display()))?;
    let mut stdout = child.stdout.take().context("auth status stdout")?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(&mut stdout, 256 * 1024), &mut out);
        out
    });
    let deadline = Instant::now() + STATUS_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("claude auth status did not finish in {}s", STATUS_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = reader.join().unwrap_or_default();
    // Logged out exits 1 with the same JSON shape.
    let payload: Value = serde_json::from_slice(&out)
        .map_err(|_| anyhow::anyhow!("unexpected claude auth status output (withheld); check the Claude CLI version"))?;
    Ok(parse_auth_status(&payload))
}

/// API accounts: whether a key is stored (no request is made; the key is
/// checked by the first turn).
pub fn api_status(account: &ClaudeAccount) -> ClaudeAccountStatus {
    let present = api_key_present(&account.id);
    let key_env = account.api.as_ref().map(|api| api.key_env).unwrap_or_default();
    ClaudeAccountStatus {
        logged_in: present,
        auth_method: Some("api_key".into()),
        api_provider: Some(
            match account.api.as_ref().and_then(|api| api.base_url.as_deref()) {
                None => "firstParty",
                Some(_) => "anthropicCompatible",
            }
            .into(),
        ),
        api_key_source: present.then(|| key_env.as_str().to_string()),
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        error: (!present).then(|| "no API key stored".to_string()),
        ..Default::default()
    }
}

/// Check an account now and remember the result (`lastStatus`).
/// Subscription / default: `claude auth status` with the account home.
pub fn check_status(id: &str) -> anyhow::Result<ClaudeAccountStatus> {
    let registry = load()?;
    let status = if id == DEFAULT_ACCOUNT_ID {
        let deployment = crate::runtime::claude::ClaudeDeployment::selected()?;
        run_auth_status(&deployment.claude_cli, &deployment.claude_home)
            .unwrap_or_else(|error| failed_status(&error))
    } else {
        let account = registry.find(id).with_context(|| format!("no Claude account {id}"))?;
        match account.kind {
            ClaudeAccountKind::Api => api_status(account),
            ClaudeAccountKind::Subscription => {
                let deployment = crate::runtime::claude::ClaudeDeployment::selected()?;
                run_auth_status(&deployment.claude_cli, &account_home(id)?)
                    .unwrap_or_else(|error| failed_status(&error))
            }
        }
    };
    let saved = status.clone();
    update_registry(|registry| {
        if id == DEFAULT_ACCOUNT_ID {
            registry.default_status = Some(saved);
        } else if let Some(account) = registry.accounts.iter_mut().find(|account| account.id == id) {
            account.last_status = Some(saved);
        }
        Ok(())
    })?;
    Ok(status)
}

fn failed_status(error: &anyhow::Error) -> ClaudeAccountStatus {
    ClaudeAccountStatus {
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        error: Some(format!("{error:#}").chars().take(400).collect()),
        ..Default::default()
    }
}

fn ensure_subscription(id: &str) -> anyhow::Result<PathBuf> {
    if id == DEFAULT_ACCOUNT_ID {
        return Ok(crate::runtime::claude::ClaudeDeployment::selected()?.claude_home);
    }
    let registry = load()?;
    let account = registry.find(id).with_context(|| format!("no Claude account {id}"))?;
    ensure!(
        account.kind == ClaudeAccountKind::Subscription,
        "{} is an API account; it has no login (set its key with cutex human claude account set {} --api-key-stdin)",
        account.label,
        account.id
    );
    let home = account_home(id)?;
    crate::management::client_tokens::ensure_private_dir(&home)?;
    Ok(home)
}

fn quote_powershell(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn quote_posix(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The terminal command that logs a subscription account in: the cutex
/// command, and the official CLI with the account's `CLAUDE_CONFIG_DIR`
/// for PowerShell and POSIX shells. Desktop shows these; it never runs a
/// login itself (owner decision 2).
pub fn login_command(id: &str) -> anyhow::Result<Value> {
    let home = if id == DEFAULT_ACCOUNT_ID {
        crate::runtime::claude::ClaudeDeployment::selected()?.claude_home
    } else {
        let registry = load()?;
        let account = registry.find(id).with_context(|| format!("no Claude account {id}"))?;
        ensure!(account.kind == ClaudeAccountKind::Subscription, "API accounts have no login");
        account_home(id)?
    };
    let cli = crate::runtime::claude::ClaudeDeployment::selected()
        .map(|deployment| deployment.claude_cli)
        .unwrap_or_else(|_| PathBuf::from("claude"));
    let (home_text, cli_text) = (home.to_string_lossy(), cli.to_string_lossy());
    Ok(json!({
        "cutex": format!("cutex human claude account login {id}"),
        "powershell": format!(
            "$env:CLAUDE_CONFIG_DIR = {}; & {} auth login --claudeai",
            quote_powershell(&home_text),
            quote_powershell(&cli_text)
        ),
        "posix": format!(
            "CLAUDE_CONFIG_DIR={} {} auth login --claudeai",
            quote_posix(&home_text),
            quote_posix(&cli_text)
        ),
        "claudeConfigDir": home,
    }))
}

/// Run the official login in the current terminal (inherits stdio: browser
/// callback or pasted code). Then check and store the status.
pub fn run_login(id: &str, email: Option<&str>) -> anyhow::Result<ClaudeAccountStatus> {
    let home = ensure_subscription(id)?;
    let cli = crate::runtime::claude::ClaudeDeployment::selected()?.claude_cli;
    let mut command = official_command(&cli, &home);
    command.args(["auth", "login", "--claudeai"]);
    if let Some(email) = email {
        ensure!(
            !email.is_empty() && email.len() <= 254 && !email.chars().any(|c| c.is_control() || c.is_whitespace()),
            "invalid email"
        );
        command.arg("--email").arg(email);
    }
    let status = command.status().with_context(|| format!("run {} auth login", cli.display()))?;
    ensure!(status.success(), "claude auth login exited {status}");
    check_status(id)
}

/// `claude auth logout` for a subscription account (removes its login from
/// its home; agents on it fail at their next connect until a new login).
pub fn run_logout(id: &str) -> anyhow::Result<ClaudeAccountStatus> {
    let home = ensure_subscription(id)?;
    let cli = crate::runtime::claude::ClaudeDeployment::selected()?.claude_cli;
    let mut command = official_command(&cli, &home);
    command
        .args(["auth", "logout"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let status = command.status().with_context(|| format!("run {} auth logout", cli.display()))?;
    ensure!(status.success(), "claude auth logout exited {status}");
    check_status(id)
}

// ---------------------------------------------------------------------------
// Presentation (Management v2 and the CLI)
// ---------------------------------------------------------------------------

/// One account as `GET /v2/claude/accounts` shows it (no secret).
pub fn item_json(registry: &ClaudeAccountsFile, id: &str, usage: Option<&AccountUsage>) -> Option<Value> {
    let empty = AccountUsage::default();
    let usage = usage.unwrap_or(&empty);
    let sessions = json!({
        "active": usage.sessions.iter().filter(|s| s.lifecycle == "active").count(),
        "online": usage.sessions.iter().filter(|s| s.online).count(),
        "retired": usage.sessions.iter().filter(|s| s.lifecycle == "retired").count(),
    });
    if id == DEFAULT_ACCOUNT_ID {
        let home = crate::runtime::claude::ClaudeDeployment::selected().ok().map(|d| d.claude_home);
        return Some(json!({
            "id": DEFAULT_ACCOUNT_ID,
            "label": Value::Null,
            "kind": "subscription",
            "isDefault": true,
            "home": home,
            "createdAt": Value::Null,
            "api": Value::Null,
            "status": registry.default_status,
            "loginCommand": login_command(DEFAULT_ACCOUNT_ID).ok(),
            "profiles": usage.profiles,
            "sessions": sessions,
            "removable": false,
        }));
    }
    let account = registry.find(id)?;
    let api = account.api.as_ref().map(|api| {
        json!({
            "baseUrl": api.base_url,
            "keyEnv": api.key_env.as_str(),
            "env": api.env,
            "apiKeyPresent": api_key_present(&account.id),
        })
    });
    let status = match account.kind {
        // Key presence is a local fact: always current.
        ClaudeAccountKind::Api => Some(api_status(account)),
        ClaudeAccountKind::Subscription => account.last_status.clone(),
    };
    Some(json!({
        "id": account.id,
        "label": account.label,
        "kind": account.kind.as_str(),
        "isDefault": false,
        "home": account_home(&account.id).ok(),
        "createdAt": account.created_at,
        "api": api,
        "status": status,
        "loginCommand": (account.kind == ClaudeAccountKind::Subscription)
            .then(|| login_command(&account.id).ok())
            .flatten(),
        "profiles": usage.profiles,
        "sessions": sessions,
        "removable": usage.profiles.is_empty() && !usage.sessions.iter().any(|s| s.lifecycle == "active"),
    }))
}

/// `{contractVersion, accounts}`: `default` first, then the added accounts.
pub fn list_json() -> anyhow::Result<Value> {
    let registry = load()?;
    let usage = usage_by_account(&registry);
    let mut items = vec![item_json(&registry, DEFAULT_ACCOUNT_ID, usage.get(DEFAULT_ACCOUNT_ID)).expect("default")];
    for account in &registry.accounts {
        if let Some(item) = item_json(&registry, &account.id, usage.get(&account.id)) {
            items.push(item);
        }
    }
    Ok(json!({ "contractVersion": 2, "accounts": items }))
}

/// Short presentation of a session's account for session resources (no
/// status fields: a status check must not change every session resource;
/// clients join on `id` with `GET /v2/claude/accounts`). `kind` is
/// `removed` for a home whose account was deregistered.
pub fn session_account_json(registry: &ClaudeAccountsFile, record: &CutexSessionRecord) -> Option<Value> {
    let id = session_account_id(registry, record)?;
    let account = registry.find(&id);
    let kind = match account {
        Some(account) => account.kind.as_str(),
        None if id == DEFAULT_ACCOUNT_ID => "subscription",
        None => "removed",
    };
    Some(json!({
        "id": id,
        "label": account.map(|account| account.label.clone()),
        "kind": kind,
        "isDefault": id == DEFAULT_ACCOUNT_ID,
    }))
}

#[cfg(test)]
#[path = "claude_accounts_tests.rs"]
mod tests;
