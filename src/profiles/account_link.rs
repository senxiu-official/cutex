//! Owner-only profile linkage. No identity/key/token cache, network or events.
//! Claude subscription identity comes exclusively from its account settings.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use super::model::{CliKind, CodezConfig, StoredAccount};

const SECRET: &str = "runtime/profile-link/secret-v1";
const DISABLED: &str = "runtime/profile-link/disabled-v1";
const DISABLED_CONTENT: &[u8] = b"cutex-profile-link-disabled/v1\n";
const DOMAIN: &[u8] = b"cutex-profile-link/v1\0";
const MAX_IDENTITY_FILE: u64 = 16 * 1024 * 1024;

/// Offline preflight for original public 0.1.5/r252 only. Those binaries do
/// not protect this new private directory through generic file APIs and can
/// clone unknown profile metadata or leave its revision clocks stale. Never
/// inspect secret bytes, relocate plaintext, discard links, or restore auth.
/// A caller must refuse downgrade when this returns any blocker (or errors).
pub fn legacy_downgrade_blockers(home: &Path) -> Result<Vec<&'static str>> {
    let mut blockers = Vec::new();
    // Refuse an indirect private directory before inspecting any descendant.
    match fs::symlink_metadata(home.join("runtime")) {
        Ok(metadata) => ensure!(
            !linked(&metadata) && metadata.is_dir(),
            "legacy downgrade runtime metadata cannot be safely inspected"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let private = home.join("runtime/profile-link");
    match fs::symlink_metadata(&private) {
        Ok(metadata) => {
            let mut unsafe_private = linked(&metadata) || !metadata.is_dir();
            if !unsafe_private {
                for entry in fs::read_dir(&private)? {
                    let entry = entry?;
                    let metadata = fs::symlink_metadata(entry.path())?;
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    let harmless = name == "audit.jsonl"
                        || (name.ends_with(".lock") && metadata.is_file() && metadata.len() == 0)
                        || name == "disabled-v1";
                    if !harmless || linked(&metadata) || !metadata.is_file() {
                        unsafe_private = true;
                    }
                }
                // Keep disable intent, but fail closed on malformed metadata.
                if read_disabled(home).is_err() {
                    unsafe_private = true;
                }
            }
            if unsafe_private {
                blockers.push("profile_link_private_state");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let path = home.join("accounts.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(blockers),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        !linked(&metadata) && metadata.is_file() && metadata.len() <= MAX_IDENTITY_FILE,
        "legacy downgrade account metadata cannot be safely inspected"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    ensure!(
        !linked(&opened) && opened.is_file() && opened.len() <= MAX_IDENTITY_FILE,
        "legacy downgrade account metadata changed while opening"
    );
    let mut bytes = Vec::new();
    file.take(MAX_IDENTITY_FILE + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_IDENTITY_FILE,
        "legacy downgrade account metadata exceeds limit"
    );
    let accounts: crate::profiles::model::AccountsStore = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("legacy downgrade account metadata is unreadable"))?;
    ensure!(
        accounts.version == crate::profiles::model::STORE_VERSION,
        "legacy downgrade account store version is unsupported"
    );
    if accounts
        .accounts
        .iter()
        .any(|account| account.options.account_link.is_some())
    {
        blockers.push("profile_manual_link_metadata");
    }
    if accounts
        .accounts
        .iter()
        .any(|account| !account.options.appearance_updated_at.is_empty())
    {
        blockers.push("profile_appearance_clock_metadata");
    }
    Ok(blockers)
}

fn linked(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// The supplied config root is trusted; components beneath it cannot be links.
fn storage_path(home: &Path, relative: &str) -> Result<PathBuf> {
    let mut path = home
        .canonicalize()
        .context("profile link root unavailable")?;
    for component in Path::new(relative).components() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(!linked(&meta), "linked profile link storage"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => anyhow::bail!("profile link storage unavailable"),
        }
    }
    Ok(path)
}

fn secret_path(home: &Path) -> Result<PathBuf> {
    storage_path(home, SECRET)
}

fn open_private(home: &Path, relative: &str) -> Result<Option<fs::File>> {
    let path = storage_path(home, relative)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("profile link secret unavailable"),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && !linked(&meta),
        "invalid profile link secret file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "profile link secret is not owner-private"
        );
    }
    #[cfg(windows)]
    crate::platform::private_fs::validate_private_file(&file)
        .map_err(|_| anyhow::anyhow!("profile link secret is not owner-private"))?;
    Ok(Some(file))
}

fn open_secret(home: &Path) -> Result<Option<fs::File>> {
    open_private(home, SECRET)
}

fn read_disabled(home: &Path) -> Result<bool> {
    let Some(file) = open_private(home, DISABLED)? else {
        return Ok(false);
    };
    ensure!(
        file.metadata()?.len() == DISABLED_CONTENT.len() as u64,
        "invalid profile link disabled marker"
    );
    let mut bytes = Vec::new();
    file.take(DISABLED_CONTENT.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes == DISABLED_CONTENT,
        "invalid profile link disabled marker"
    );
    Ok(true)
}

fn enabled_secret(home: &Path) -> Result<Option<[u8; 32]>> {
    if read_disabled(home)? {
        return Ok(None);
    }
    read_secret(home)
}

fn read_secret_bytes(mut file: fs::File) -> Result<[u8; 32]> {
    ensure!(
        file.metadata()?.len() == 32,
        "invalid profile link secret length"
    );
    let mut bytes = [0; 32];
    file.read_exact(&mut bytes)
        .context("profile link secret unreadable")?;
    let mut extra = [0; 1];
    ensure!(
        file.read(&mut extra)? == 0,
        "profile link secret changed while reading"
    );
    Ok(bytes)
}

fn read_secret(home: &Path) -> Result<Option<[u8; 32]>> {
    open_secret(home)?.map(read_secret_bytes).transpose()
}

// Local repair/delete may replace a malformed but owner-private regular file.
// Its prior ID is unknown; it still counts as present for initialize-only.
fn prior_secret(home: &Path) -> Result<(bool, Option<[u8; 32]>)> {
    let Some(file) = open_secret(home)? else {
        return Ok((false, None));
    };
    if file.metadata()?.len() != 32 {
        return Ok((true, None));
    }
    Ok((true, Some(read_secret_bytes(file)?)))
}

fn secret_id(secret: &[u8; 32]) -> String {
    Sha256::digest(secret)[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Metadata only: never return the secret itself.
pub fn secret_status(home: &Path) -> Result<Value> {
    if read_disabled(home)? {
        return Ok(json!({"secretId":null,"disabled":true}));
    }
    Ok(json!({"secretId": read_secret(home)?.as_ref().map(secret_id)}))
}

fn decode_exact(value: &str, bytes: usize) -> Result<Vec<u8>> {
    let expected = (bytes * 8).div_ceil(6);
    ensure!(value.len() == expected, "invalid profile link value length");
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| anyhow::anyhow!("invalid profile link encoding"))?;
    ensure!(
        decoded.len() == bytes && URL_SAFE_NO_PAD.encode(&decoded) == value,
        "invalid profile link encoding"
    );
    Ok(decoded)
}

#[derive(Clone, Copy)]
pub enum PutPolicy {
    InitializeOnly,
    Replace,
}

#[derive(Debug)]
pub struct ReplacementDenied;
impl std::fmt::Display for ReplacementDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("profile link secret replacement requires owner-local access")
    }
}
impl std::error::Error for ReplacementDenied {}

pub struct SecretChange {
    pub secret_id_before: Option<String>,
    pub secret_id_after: Option<String>,
    pub changed: bool,
    pub disabled: bool,
}
impl SecretChange {
    pub fn status(&self) -> Value {
        let mut status = json!({"secretId":self.secret_id_after});
        if self.disabled {
            status["disabled"] = json!(true);
        }
        status
    }
}

/// A small sanity rule, not an entropy estimate. Accept OS-random bytes; refuse
/// common human-entered, repeated and sequential placeholders.
fn validate_secret_quality(bytes: &[u8]) -> Result<()> {
    let distinct: BTreeSet<_> = bytes.iter().copied().collect();
    let repeating = (1..=16).any(|period| {
        bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte == bytes[index % period])
    });
    let step = bytes[1].wrapping_sub(bytes[0]);
    let sequential = bytes
        .windows(2)
        .all(|pair| pair[1].wrapping_sub(pair[0]) == step);
    ensure!(
        distinct.len() >= 16 && !repeating && !sequential && !bytes.is_ascii(),
        "profile link secret must be 32 cryptographically random bytes"
    );
    Ok(())
}

fn mutation_lock(home: &Path) -> Result<fs::File> {
    let path = secret_path(home)?;
    let directory = path.parent().unwrap();
    crate::management::client_tokens::ensure_private_dir(directory)?;
    secret_path(home)?;
    let lock_path = directory.join("secret-v1.lock");
    if let Ok(meta) = fs::symlink_metadata(&lock_path) {
        ensure!(
            meta.is_file() && !linked(&meta),
            "invalid profile link secret lock"
        );
    }
    #[cfg(windows)]
    let file = crate::platform::private_fs::open_private_file_path(&lock_path, true, false)
        .map_err(|_| anyhow::anyhow!("profile link secret lock unavailable"))?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "profile link secret lock is not owner-private"
        );
        file
    };
    fs2::FileExt::lock_exclusive(&file).context("profile link secret lock unavailable")?;
    Ok(file)
}

/// Canonical encoding and heuristic quality check; never echoes secret input.
pub fn validate_secret(value: &str) -> Result<()> {
    validate_secret_quality(&decode_exact(value, 32)?)
}

/// The existence decision and mutation share the same cross-process lock.
/// InitializeOnly rejects even an identical value when a secret already exists.
pub fn put_secret_with_policy(
    home: &Path,
    secret: &str,
    policy: PutPolicy,
) -> Result<SecretChange> {
    let bytes = decode_exact(secret, 32)?;
    validate_secret_quality(&bytes)?;
    let _lock = mutation_lock(home)?;
    let path = secret_path(home)?;
    // Refuse paired replacement before reading possibly damaged prior contents.
    let marker = storage_path(home, DISABLED)?;
    if matches!(policy, PutPolicy::InitializeOnly)
        && (fs::symlink_metadata(&path).is_ok() || fs::symlink_metadata(&marker).is_ok())
    {
        return Err(ReplacementDenied.into());
    }
    // Explicit local Replace may repair malformed marker contents, but only
    // after validating a private regular non-linked file.
    let was_disabled = open_private(home, DISABLED)?.is_some();
    let (_, before) = prior_secret(home)?;
    let changed = was_disabled
        || before
            .as_ref()
            .is_none_or(|previous| previous.as_slice() != bytes.as_slice());
    let after: [u8; 32] = bytes.as_slice().try_into().unwrap();
    if changed {
        crate::config::atomic::write_private_bytes_atomic(&path, &bytes)?;
    }
    // Clear off intent LAST: a crash after writing the secret stays disabled.
    if was_disabled {
        fs::remove_file(marker)?;
        #[cfg(unix)]
        fs::File::open(path.parent().unwrap())?.sync_all()?;
    }
    Ok(SecretChange {
        secret_id_before: if was_disabled {
            None
        } else {
            before.as_ref().map(secret_id)
        },
        secret_id_after: Some(secret_id(&after)),
        changed,
        disabled: false,
    })
}

pub fn put_secret(home: &Path, secret: &str) -> Result<Value> {
    Ok(put_secret_with_policy(home, secret, PutPolicy::Replace)?.status())
}

pub fn delete_secret_change(home: &Path) -> Result<SecretChange> {
    let _lock = mutation_lock(home)?;
    let was_disabled = read_disabled(home)?;
    let (present, before) = prior_secret(home)?;
    // Persist off intent FIRST: even a crash or unlink error cannot permit
    // another paired desktop to initialize or use the old secret again.
    if !was_disabled {
        crate::config::atomic::write_private_bytes_atomic(
            &storage_path(home, DISABLED)?,
            DISABLED_CONTENT,
        )?;
    }
    if present {
        fs::remove_file(secret_path(home)?)?;
        #[cfg(unix)]
        fs::File::open(secret_path(home)?.parent().unwrap())?.sync_all()?;
    }
    Ok(SecretChange {
        secret_id_before: if was_disabled {
            None
        } else {
            before.as_ref().map(secret_id)
        },
        secret_id_after: None,
        changed: !was_disabled || present,
        disabled: true,
    })
}

pub fn secret_delete(home: &Path) -> Result<Value> {
    Ok(delete_secret_change(home)?.status())
}

/// Only an explicit opt-out or a canonical random 128-bit manual token.
pub fn validate_manual(value: &str) -> Result<String> {
    if value == "none" {
        return Ok(value.into());
    }
    let token = value
        .strip_prefix("m-")
        .context("invalid manual profile link")?;
    decode_exact(token, 16)?;
    Ok(value.into())
}

// RFC 2104, SHA-256's 64-byte compression block. This helper also handles long
// keys so RFC 4231 test vectors exercise the complete standard construction.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn link(secret: &[u8; 32], family: &str, basis: &str, identity: &str) -> Option<Value> {
    if family.is_empty() || family.contains('\0') {
        return None;
    }
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(family.as_bytes());
    message.push(0);
    message.extend_from_slice(basis.as_bytes());
    message.push(0);
    message.extend_from_slice(identity.as_bytes());
    let encoded = URL_SAFE_NO_PAD.encode(hmac_sha256(secret, &message));
    Some(json!({"accountKey": &encoded[..22], "basis":basis,
        "source":if basis == "manual" {"manual"} else {"auto"}, "secretId":secret_id(secret)}))
}

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|text| !text.trim().is_empty() && !text.chars().any(char::is_control))
}

fn chatgpt_identity(auth: &Value) -> Option<String> {
    let token = auth.pointer("/tokens/id_token")?.as_str()?;
    let claims = super::import::id_token_claims(token)?;
    let account = nonempty(claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")?)?;
    let user = claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_user_id")
        .and_then(nonempty)?;
    Some(format!("{account}\0{user}"))
}

fn bounded_json(path: &Path) -> Option<Value> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || linked(&metadata) || metadata.len() > MAX_IDENTITY_FILE {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_IDENTITY_FILE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_IDENTITY_FILE {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
fn automatic_link(
    secret: &[u8; 32],
    family: &str,
    auth: Option<&Value>,
    claude_settings: Option<&Value>,
    api_key: Option<&str>,
) -> Option<Value> {
    if auth.is_some_and(|auth| {
        auth.get("auth_mode").and_then(Value::as_str) == Some("chatgpt")
            || auth.get("tokens").is_some_and(Value::is_object)
    }) {
        return link(secret, family, "chatgpt-user", &chatgpt_identity(auth?)?);
    }
    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        if !api_link_allowed(None, key) {
            return None;
        }
        return link(secret, family, "api-key", key);
    }
    if let Some(settings) = claude_settings {
        let id = nonempty(settings.pointer("/oauthAccount/accountUuid")?)?;
        return link(secret, family, "claude-account", id);
    }
    link(secret, family, "chatgpt-user", &chatgpt_identity(auth?)?)
}

/// Explicit root and lazy identity input keep synthetic tests independent of
/// global account registries and auth paths. Manual/none never resolve auth.
#[cfg(test)]
fn profile_link_with(
    home: &Path,
    account: &StoredAccount,
    family: impl FnOnce() -> Option<String>,
    automatic: impl FnOnce(&[u8; 32], &str) -> Option<Value>,
) -> Option<Value> {
    if account.options.account_link.as_deref() == Some("none") {
        return None;
    }
    let secret = enabled_secret(home).ok()??;
    let family = family()?;
    if let Some(manual) = account.options.account_link.as_deref() {
        validate_manual(manual).ok()?;
        return link(&secret, &family, "manual", manual.strip_prefix("m-")?);
    }
    automatic(&secret, &family)
}

fn api_link_allowed(endpoint: Option<&str>, key: &str) -> bool {
    if key.trim().len() < 16 {
        return false;
    }
    let Some(endpoint) = endpoint else {
        return true;
    };
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Ipv4(ip)) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1])))
        }
        Some(url::Host::Ipv6(ip)) => ip.to_ipv4_mapped().map_or_else(
            || {
                !(ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
                    || ip.is_multicast())
            },
            |v4| api_link_allowed(Some(&format!("http://{v4}")), key),
        ),
        Some(url::Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host.contains('.')
                && !["localhost", "local", "internal", "lan", "home", "home.arpa"]
                    .iter()
                    .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
        }
        None => false,
    }
}

struct ClaudeIdentity {
    basis: &'static str,
    value: String,
}

/// One owner HTTP projection only. Never retain this context between requests:
/// it holds ephemeral identity inputs, not a persistent or process-global cache.
pub struct LinkContext {
    secret: Option<[u8; 32]>,
    registry: Option<crate::runtime::claude_accounts::ClaudeAccountsFile>,
    default_home: Option<PathBuf>,
    claude: BTreeMap<PathBuf, Option<ClaudeIdentity>>,
}
impl LinkContext {
    pub fn new() -> Self {
        let secret = crate::config::paths::config_dir()
            .ok()
            .and_then(|home| enabled_secret(&home).ok().flatten());
        Self {
            secret,
            registry: if secret.is_some() {
                crate::runtime::claude_accounts::load().ok()
            } else {
                None
            },
            default_home: if secret.is_some() {
                crate::runtime::claude::ClaudeDeployment::selected()
                    .ok()
                    .map(|deployment| deployment.claude_home)
            } else {
                None
            },
            claude: BTreeMap::new(),
        }
    }

    pub fn profile_link(
        &mut self,
        account: &StoredAccount,
        _config: &CodezConfig,
    ) -> Option<Value> {
        if account.options.account_link.as_deref() == Some("none") {
            return None;
        }
        let secret = self.secret?;
        if account.cli_kind != CliKind::Claude {
            let family = super::providers::provider_family(account).0;
            if let Some(manual) = account.options.account_link.as_deref() {
                validate_manual(manual).ok()?;
                return link(&secret, &family, "manual", manual.strip_prefix("m-")?);
            }
            let auth = super::inspect::account_auth_payload(account)?;
            let chatgpt = account.source.as_deref() == Some("official")
                || auth.get("auth_mode").and_then(Value::as_str) == Some("chatgpt")
                || auth.get("tokens").is_some_and(Value::is_object);
            if chatgpt {
                return link(&secret, &family, "chatgpt-user", &chatgpt_identity(&auth)?);
            }
            let key = super::catalog_api::api_key(account).ok()?;
            let endpoint = super::inspect::account_model_api_base(account);
            if !api_link_allowed(endpoint.as_deref(), &key) {
                return None;
            }
            return link(&secret, &family, "api-key", key.trim());
        }
        let id = account
            .claude_account
            .as_deref()
            .unwrap_or(crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID);
        let entry = if id == crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID {
            None
        } else {
            self.registry
                .as_ref()
                .and_then(|registry| registry.find(id))
        };
        let family = if id != crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID && entry.is_none()
        {
            format!("custom:unresolved-claude-account:{id}")
        } else {
            super::providers::family_for(
                &CliKind::Claude,
                None,
                entry
                    .and_then(|entry| entry.api.as_ref())
                    .and_then(|api| api.base_url.as_deref()),
            )
            .0
        };
        if let Some(manual) = account.options.account_link.as_deref() {
            validate_manual(manual).ok()?;
            return link(&secret, &family, "manual", manual.strip_prefix("m-")?);
        }
        let (native, api) = if id == crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID {
            (self.default_home.clone()?, None)
        } else {
            let entry = entry?;
            (
                crate::runtime::claude_accounts::account_home(id).ok()?,
                (entry.kind == crate::runtime::claude_accounts::ClaudeAccountKind::Api)
                    .then(|| entry.api.clone().unwrap_or_default()),
            )
        };
        if !self.claude.contains_key(&native) {
            let identity = if let Some(api) = api {
                let payload = bounded_json(&crate::runtime::claude_accounts::key_path(id).ok()?);
                payload.and_then(|payload| {
                    let key = payload.get("apiKey")?.as_str()?;
                    if !api_link_allowed(api.base_url.as_deref(), key) {
                        return None;
                    }
                    Some(ClaudeIdentity {
                        basis: "api-key",
                        value: key.trim().into(),
                    })
                })
            } else {
                bounded_json(&native.join(".claude.json")).and_then(|settings| {
                    Some(ClaudeIdentity {
                        basis: "claude-account",
                        value: nonempty(settings.pointer("/oauthAccount/accountUuid")?)?.into(),
                    })
                })
            };
            self.claude.insert(native.clone(), identity);
        }
        let identity = self.claude.get(&native)?.as_ref()?;
        link(&secret, &family, identity.basis, &identity.value)
    }
}
impl Default for LinkContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience for a single owner detail response. Lists should reuse a context.
pub fn profile_link(account: &StoredAccount, config: &CodezConfig) -> Option<Value> {
    LinkContext::new().profile_link(account, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_secret(seed: u8) -> [u8; 32] {
        Sha256::digest([seed; 32]).into()
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("profile-link-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn put(&self, byte: u8) -> Value {
            put_secret(&self.0, &URL_SAFE_NO_PAD.encode(synthetic_secret(byte))).unwrap()
        }
        fn derive(&self, family: &str, auth: &Value) -> Option<Value> {
            automatic_link(&read_secret(&self.0).ok()??, family, Some(auth), None, None)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn auth(account: Option<&str>, user: Option<&str>, sub: Option<&str>) -> Value {
        let claims = json!({"https://api.openai.com/auth":{
            "chatgpt_account_id":account, "chatgpt_user_id":user}, "sub":sub});
        json!({"tokens":{"id_token":format!("e30.{}.synthetic", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()))}})
    }

    #[test]
    fn rfc4231_hmac_vectors() {
        let hex = |bytes: [u8; 32]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            hex(hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn secret_status_reset_and_validation_never_return_the_secret() {
        let f = Fixture::new();
        assert_eq!(secret_status(&f.0).unwrap(), json!({"secretId":null}));
        assert!(f
            .derive(
                "openai-chatgpt",
                &auth(Some("workspace"), Some("member"), None)
            )
            .is_none());
        let status = f.put(7);
        assert_eq!(status.as_object().unwrap().len(), 1);
        assert_eq!(status["secretId"].as_str().unwrap().len(), 8);
        assert_eq!(fs::read(f.0.join(SECRET)).unwrap(), synthetic_secret(7));
        let first = f
            .derive(
                "openai-chatgpt",
                &auth(Some("workspace"), Some("member"), None),
            )
            .unwrap();
        assert_ne!(f.put(9), status);
        let second = f
            .derive(
                "openai-chatgpt",
                &auth(Some("workspace"), Some("member"), None),
            )
            .unwrap();
        assert_ne!(first["accountKey"], second["accountKey"]);
        for invalid in [
            "secret-material",
            "",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "../secret-material",
        ] {
            let error = put_secret(&f.0, invalid).unwrap_err().to_string();
            assert!(!error.contains("secret-material"));
        }
    }

    #[test]
    fn chatgpt_team_members_and_workspaces_remain_distinct_and_missing_ids_do_not_link() {
        let f = Fixture::new();
        f.put(1);
        let original = f
            .derive("openai-chatgpt", &auth(Some("team"), Some("one"), None))
            .unwrap();
        assert_eq!(
            original,
            f.derive(
                "openai-chatgpt",
                &auth(Some("team"), Some("one"), Some("other-sub"))
            )
            .unwrap()
        );
        assert_ne!(
            original,
            f.derive("openai-chatgpt", &auth(Some("team"), Some("two"), None))
                .unwrap()
        );
        assert_ne!(
            original,
            f.derive(
                "openai-chatgpt",
                &auth(Some("other-team"), Some("one"), None)
            )
            .unwrap()
        );
        for missing in [
            auth(None, Some("one"), None),
            auth(Some("team"), None, None),
            auth(Some("team"), Some(""), None),
        ] {
            assert!(f.derive("openai-chatgpt", &missing).is_none());
        }
        assert!(f
            .derive("openai-chatgpt", &auth(Some("team"), None, Some("one")))
            .is_none());
    }

    #[test]
    fn claude_account_uuid_not_organization_and_api_provider_families_are_isolated() {
        let secret = [5; 32];
        let settings =
            |user| json!({"oauthAccount":{"accountUuid":user,"organizationUuid":"shared-team"}});
        let first = automatic_link(
            &secret,
            "anthropic",
            None,
            Some(&settings("member-one")),
            None,
        )
        .unwrap();
        assert_ne!(
            first,
            automatic_link(
                &secret,
                "anthropic",
                None,
                Some(&settings("member-two")),
                None
            )
            .unwrap()
        );
        assert!(automatic_link(
            &secret,
            "anthropic",
            None,
            Some(&json!({"oauthAccount":{"organizationUuid":"shared-team"}})),
            None
        )
        .is_none());
        let api =
            automatic_link(&secret, "zhipu", None, None, Some("synthetic-provider-key")).unwrap();
        assert_eq!(
            api,
            automatic_link(&secret, "zhipu", None, None, Some("synthetic-provider-key")).unwrap()
        );
        assert_ne!(
            api,
            automatic_link(
                &secret,
                "moonshot",
                None,
                None,
                Some("synthetic-provider-key")
            )
            .unwrap()
        );
        assert_ne!(
            api,
            automatic_link(&secret, "zhipu", None, None, Some("other-provider-key")).unwrap()
        );
        assert_eq!(api["basis"], "api-key");
    }

    #[test]
    fn manual_validation_and_namespace_do_not_expose_manual_tokens() {
        let manual = format!("m-{}", URL_SAFE_NO_PAD.encode([3; 16]));
        assert_eq!(validate_manual(&manual).unwrap(), manual);
        assert_eq!(validate_manual("none").unwrap(), "none");
        for value in [
            "",
            "NONE",
            "m-not-a-token",
            "m-AAAAAAAAAAAAAAAAAAAAAB",
            " m-AAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(validate_manual(value).is_err());
        }
        let token = manual.strip_prefix("m-").unwrap();
        let result = link(&[4; 32], "anthropic", "manual", token).unwrap();
        assert_eq!(result["source"], "manual");
        assert!(!result.to_string().contains(&manual));
        assert!(!result.to_string().contains(token));
        assert_ne!(
            result["accountKey"],
            link(&[4; 32], "anthropic", "api-key", token).unwrap()["accountKey"]
        );
    }

    #[test]
    fn manual_priority_opt_out_missing_secret_and_invalid_options_never_resolve_identity() {
        let f = Fixture::new();
        let mut account =
            super::super::lifecycle::base_account("synthetic", CliKind::Codex, "official");
        let family = || Some("openai-chatgpt".to_string());
        assert!(profile_link_with(&f.0, &account, family, |_, _| panic!(
            "missing secret must not read auth"
        ))
        .is_none());
        f.put(1);
        account.options.account_link = Some(format!("m-{}", URL_SAFE_NO_PAD.encode([5; 16])));
        let manual = profile_link_with(&f.0, &account, family, |_, _| {
            panic!("manual must override auto")
        })
        .unwrap();
        assert_eq!(manual["basis"], "manual");
        account.options.account_link = Some("none".into());
        assert!(profile_link_with(
            &f.0,
            &account,
            || panic!("opt-out must not inspect provider"),
            |_, _| panic!("opt-out must not read auth")
        )
        .is_none());
        account.options.account_link = Some("invalid".into());
        assert!(profile_link_with(&f.0, &account, family, |_, _| panic!(
            "invalid manual must not fall back to auto"
        ))
        .is_none());
        account.options.account_link = None;
        let auto = profile_link_with(&f.0, &account, family, |secret, family| {
            automatic_link(
                secret,
                family,
                Some(&auth(Some("workspace"), Some("person"), None)),
                None,
                None,
            )
        })
        .unwrap();
        assert_eq!(auto["source"], "auto");
        assert_ne!(auto["accountKey"], manual["accountKey"]);
    }

    #[test]
    fn identity_resolution_is_read_only_and_no_raw_identifiers_are_persisted() {
        let f = Fixture::new();
        f.put(2);
        let settings_path = f.0.join(".claude.json");
        let settings = json!({"oauthAccount":{"accountUuid":"synthetic-person","organizationUuid":"synthetic-team"}});
        fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
        let before = fs::read(&settings_path).unwrap();
        let parsed = bounded_json(&settings_path).unwrap();
        let result = automatic_link(
            &read_secret(&f.0).unwrap().unwrap(),
            "anthropic",
            None,
            Some(&parsed),
            None,
        )
        .unwrap();
        assert_eq!(fs::read(settings_path).unwrap(), before);
        assert_eq!(result.as_object().unwrap().len(), 4);
        for raw in ["synthetic-person", "synthetic-team"] {
            assert!(!result.to_string().contains(raw));
        }
        assert_eq!(
            fs::read_dir(f.0.join("runtime/profile-link"))
                .unwrap()
                .count(),
            2
        );
        assert_eq!(fs::read(f.0.join(SECRET)).unwrap(), synthetic_secret(2));
        // No cache: a changed identity immediately produces a changed result.
        let changed = json!({"oauthAccount":{"accountUuid":"another-person"}});
        assert_ne!(
            result,
            automatic_link(
                &synthetic_secret(2),
                "anthropic",
                None,
                Some(&changed),
                None
            )
            .unwrap()
        );
    }

    #[test]
    fn chatgpt_exchanged_keys_never_override_actual_user_claims() {
        let mut login = auth(Some("same-workspace"), Some("same-user"), None);
        login["OPENAI_API_KEY"] = json!("exchanged-session-key-one");
        login["auth_mode"] = json!("chatgpt");
        let first = automatic_link(
            &[3; 32],
            "openai-chatgpt",
            Some(&login),
            None,
            Some("exchanged-session-key-one"),
        )
        .unwrap();
        assert_eq!(first["basis"], "chatgpt-user");
        assert_eq!(
            first,
            automatic_link(
                &[3; 32],
                "openai-chatgpt",
                Some(&login),
                None,
                Some("exchanged-session-key-two")
            )
            .unwrap()
        );
        let missing = auth(Some("same-workspace"), None, Some("fallback-sub"));
        assert!(automatic_link(
            &[3; 32],
            "openai-chatgpt",
            Some(&missing),
            None,
            Some("exchanged-session-key-one")
        )
        .is_none());
        assert!(automatic_link(
            &[3; 32],
            "openai-chatgpt",
            Some(&json!({"auth_mode":"chatgpt"})),
            None,
            Some("exchanged-session-key-one")
        )
        .is_none());
    }

    #[test]
    fn private_endpoints_and_placeholder_api_keys_only_allow_manual_links() {
        let key = "synthetic-provider-api-key";
        for endpoint in [
            "http://localhost:11434/v1",
            "http://127.0.0.1/v1",
            "http://10.0.0.2/v1",
            "http://10.0.0.3/v1",
            "http://172.16.0.1/v1",
            "http://100.64.0.2/v1",
            "http://[::1]/v1",
            "http://[fd00::1]/v1",
            "http://[::ffff:127.0.0.1]/v1",
            "http://provider.local/v1",
            "http://provider.internal/v1",
        ] {
            assert!(!api_link_allowed(Some(endpoint), key), "{endpoint}");
        }
        assert!(!api_link_allowed(
            Some("https://provider.example.invalid"),
            "ollama"
        ));
        assert!(api_link_allowed(
            Some("https://provider.example.invalid"),
            key
        ));
        assert!(link(&[3; 32], "custom:localhost", "manual", "synthetic-group").is_some());
    }

    #[test]
    fn large_claude_settings_are_parsed_once_per_home_in_request_context() {
        let f = Fixture::new();
        let settings = json!({"oauthAccount":{"accountUuid":"synthetic-person"},"history":"x".repeat(2 * 1024 * 1024)});
        fs::write(
            f.0.join(".claude.json"),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        let account = super::super::lifecycle::new_claude_account("synthetic-claude");
        let mut context = LinkContext {
            secret: Some(synthetic_secret(3)),
            registry: Some(Default::default()),
            default_home: Some(f.0.clone()),
            claude: BTreeMap::new(),
        };
        let first = context
            .profile_link(&account, &CodezConfig::default())
            .unwrap();
        fs::write(f.0.join(".claude.json"), b"invalidated after first parse").unwrap();
        assert_eq!(
            first,
            context
                .profile_link(&account, &CodezConfig::default())
                .unwrap()
        );
        assert_eq!(context.claude.len(), 1);
        let mut next = LinkContext {
            secret: Some(synthetic_secret(3)),
            registry: Some(Default::default()),
            default_home: Some(f.0.clone()),
            claude: BTreeMap::new(),
        };
        assert!(next
            .profile_link(&account, &CodezConfig::default())
            .is_none());
    }

    #[test]
    fn weak_secrets_are_rejected_and_initialization_replacement_and_delete_are_explicit() {
        let f = Fixture::new();
        for bytes in [
            vec![0; 32],
            vec![9; 32],
            (0..32).collect(),
            b"0123456789abcdefghijklmnopqrstuv".to_vec(),
            (0..16).cycle().take(32).collect(),
        ] {
            let encoded = URL_SAFE_NO_PAD.encode(bytes);
            assert!(validate_secret(&encoded).is_err());
            assert!(put_secret(&f.0, &encoded).is_err());
        }
        let first = URL_SAFE_NO_PAD.encode(synthetic_secret(1));
        let created = put_secret_with_policy(&f.0, &first, PutPolicy::InitializeOnly).unwrap();
        assert!(created.changed);
        assert!(created.secret_id_before.is_none());
        let denied = put_secret_with_policy(&f.0, &first, PutPolicy::InitializeOnly)
            .err()
            .unwrap();
        assert!(denied.downcast_ref::<ReplacementDenied>().is_some());
        assert!(
            !put_secret_with_policy(&f.0, &first, PutPolicy::Replace)
                .unwrap()
                .changed
        );
        let changed = put_secret_with_policy(
            &f.0,
            &URL_SAFE_NO_PAD.encode(synthetic_secret(2)),
            PutPolicy::Replace,
        )
        .unwrap();
        assert_eq!(changed.secret_id_before, created.secret_id_after);
        assert_ne!(changed.secret_id_before, changed.secret_id_after);
        let deleted = delete_secret_change(&f.0).unwrap();
        assert_eq!(deleted.secret_id_before, changed.secret_id_after);
        assert!(deleted.changed);
        assert_eq!(
            secret_status(&f.0).unwrap(),
            json!({"secretId":null,"disabled":true})
        );
        assert!(!delete_secret_change(&f.0).unwrap().changed);
        crate::config::atomic::write_private_bytes_atomic(
            &f.0.join(SECRET),
            b"malformed-existing-secret",
        )
        .unwrap();
        assert!(
            put_secret_with_policy(&f.0, &first, PutPolicy::InitializeOnly)
                .err()
                .unwrap()
                .downcast_ref::<ReplacementDenied>()
                .is_some()
        );
        assert_eq!(
            fs::read(f.0.join(SECRET)).unwrap(),
            b"malformed-existing-secret"
        );
        assert!(delete_secret_change(&f.0).unwrap().changed);
        assert_eq!(
            secret_status(&f.0).unwrap(),
            json!({"secretId":null,"disabled":true})
        );
    }

    #[test]
    fn durable_off_on_virgin_home_cannot_be_reseeded_by_first_writer() {
        let f = Fixture::new();
        assert_eq!(secret_status(&f.0).unwrap(), json!({"secretId":null}));
        assert!(!f.0.join("runtime").exists(), "GET must not create state");
        let off = delete_secret_change(&f.0).unwrap();
        assert!(off.changed && off.disabled);
        assert_eq!(off.status(), json!({"secretId":null,"disabled":true}));
        assert!(!f.0.join(SECRET).exists());
        let before = fs::read(f.0.join(DISABLED)).unwrap();
        assert_eq!(secret_status(&f.0).unwrap(), off.status());
        assert_eq!(fs::read(f.0.join(DISABLED)).unwrap(), before);
        let repeated = delete_secret_change(&f.0).unwrap();
        assert!(!repeated.changed && repeated.disabled);
        let secret = URL_SAFE_NO_PAD.encode(synthetic_secret(1));
        assert!(
            put_secret_with_policy(&f.0, &secret, PutPolicy::InitializeOnly)
                .err()
                .unwrap()
                .downcast_ref::<ReplacementDenied>()
                .is_some()
        );
        let enabled = put_secret_with_policy(&f.0, &secret, PutPolicy::Replace).unwrap();
        assert!(enabled.changed && !enabled.disabled);
        assert!(enabled.secret_id_before.is_none());
        assert!(!f.0.join(DISABLED).exists());
        assert!(enabled.status().get("disabled").is_none());
        assert_eq!(enabled.status(), secret_status(&f.0).unwrap());
    }

    #[test]
    fn persisted_off_marker_dominates_old_or_new_secret_during_crash_recovery() {
        let f = Fixture::new();
        f.put(1);
        // DELETE's durable first step; simulate interruption before unlink.
        crate::config::atomic::write_private_bytes_atomic(&f.0.join(DISABLED), DISABLED_CONTENT)
            .unwrap();
        assert!(f.0.join(SECRET).exists());
        assert_eq!(
            secret_status(&f.0).unwrap(),
            json!({"secretId":null,"disabled":true})
        );
        let mut account = super::super::lifecycle::new_claude_account("synthetic-off");
        account.options.account_link = Some(format!("m-{}", URL_SAFE_NO_PAD.encode([3; 16])));
        let mut fresh = LinkContext {
            secret: enabled_secret(&f.0).unwrap(),
            registry: None,
            default_home: None,
            claude: BTreeMap::new(),
        };
        assert!(fresh
            .profile_link(&account, &CodezConfig::default())
            .is_none());
        // PUT's durable first step; simulate interruption before marker removal.
        crate::config::atomic::write_private_bytes_atomic(&f.0.join(SECRET), &synthetic_secret(2))
            .unwrap();
        assert!(enabled_secret(&f.0).unwrap().is_none());
        let replacement = URL_SAFE_NO_PAD.encode(synthetic_secret(2));
        assert!(
            put_secret_with_policy(&f.0, &replacement, PutPolicy::InitializeOnly)
                .err()
                .unwrap()
                .downcast_ref::<ReplacementDenied>()
                .is_some()
        );
        let enabled = put_secret_with_policy(&f.0, &replacement, PutPolicy::Replace).unwrap();
        assert!(enabled.changed && !enabled.disabled);
        assert!(enabled_secret(&f.0).unwrap().is_some());
        assert!(!f.0.join(DISABLED).exists());
        // A completed DELETE has removed physical key material and retained off.
        assert!(delete_secret_change(&f.0).unwrap().changed);
        assert!(!f.0.join(SECRET).exists());
        assert!(read_disabled(&f.0).unwrap());
        // GET while off must not inspect even malformed stale secret contents.
        crate::config::atomic::write_private_bytes_atomic(
            &f.0.join(SECRET),
            b"malformed stale secret",
        )
        .unwrap();
        assert_eq!(
            secret_status(&f.0).unwrap(),
            json!({"secretId":null,"disabled":true})
        );
        assert!(enabled_secret(&f.0).unwrap().is_none());
    }

    #[test]
    fn invalid_disabled_marker_fails_closed_and_local_replace_can_repair_private_contents() {
        let f = Fixture::new();
        f.put(1);
        crate::config::atomic::write_private_bytes_atomic(
            &f.0.join(DISABLED),
            b"unknown marker format",
        )
        .unwrap();
        assert!(secret_status(&f.0).is_err());
        assert!(enabled_secret(&f.0).is_err());
        let replacement = URL_SAFE_NO_PAD.encode(synthetic_secret(2));
        assert!(
            put_secret_with_policy(&f.0, &replacement, PutPolicy::InitializeOnly)
                .err()
                .unwrap()
                .downcast_ref::<ReplacementDenied>()
                .is_some()
        );
        let repaired = put_secret_with_policy(&f.0, &replacement, PutPolicy::Replace).unwrap();
        assert!(repaired.changed && !repaired.disabled);
        assert!(!f.0.join(DISABLED).exists());
    }

    #[test]
    fn simultaneous_first_writers_cannot_both_initialize() {
        let f = Fixture::new();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for seed in [1, 2] {
            let root = f.0.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                put_secret_with_policy(
                    &root,
                    &URL_SAFE_NO_PAD.encode(synthetic_secret(seed)),
                    PutPolicy::InitializeOnly,
                )
            }));
        }
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.downcast_ref::<ReplacementDenied>().is_some()))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_secret_permissions_corruption_and_symlinks_are_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let f = Fixture::new();
        f.put(6);
        let path = f.0.join(SECRET);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(secret_status(&f.0).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"corrupt").unwrap();
        assert!(secret_status(&f.0).is_err());
        fs::remove_file(&path).unwrap();
        let target = f.0.join("outside-secret");
        fs::write(&target, [8; 32]).unwrap();
        symlink(&target, &path).unwrap();
        assert!(secret_status(&f.0).is_err());
        assert!(put_secret(&f.0, &URL_SAFE_NO_PAD.encode(synthetic_secret(9))).is_err());
        assert_eq!(fs::read(target).unwrap(), [8; 32]);
        fs::remove_file(path).unwrap();
        let marker = f.0.join(DISABLED);
        symlink(f.0.join("outside-secret"), &marker).unwrap();
        assert!(secret_status(&f.0).is_err());
        assert!(enabled_secret(&f.0).is_err());
        assert!(put_secret(&f.0, &URL_SAFE_NO_PAD.encode(synthetic_secret(9))).is_err());
        fs::remove_file(&marker).unwrap();
        crate::config::atomic::write_private_bytes_atomic(&marker, DISABLED_CONTENT).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(secret_status(&f.0).is_err());
        assert!(enabled_secret(&f.0).is_err());
    }
    #[test]
    fn legacy_downgrade_preflight_blocks_private_secret_state_without_reading_its_bytes() {
        let f = Fixture::new();
        assert!(legacy_downgrade_blockers(&f.0).unwrap().is_empty());
        fs::create_dir_all(f.0.join("runtime/profile-link")).unwrap();
        // A directory cannot be read as secret material, but must still block.
        fs::create_dir(f.0.join(SECRET)).unwrap();
        assert_eq!(
            legacy_downgrade_blockers(&f.0).unwrap(),
            ["profile_link_private_state"]
        );
        fs::remove_dir(f.0.join(SECRET)).unwrap();
        crate::config::atomic::write_private_bytes_atomic(&f.0.join(DISABLED), DISABLED_CONTENT)
            .unwrap();
        assert!(legacy_downgrade_blockers(&f.0).unwrap().is_empty());
        fs::write(
            f.0.join("runtime/profile-link/.secret-v1.tmp"),
            b"synthetic unfinished private state",
        )
        .unwrap();
        assert_eq!(
            legacy_downgrade_blockers(&f.0).unwrap(),
            ["profile_link_private_state"]
        );
        assert!(f.0.join(DISABLED).exists());
    }

    #[test]
    fn legacy_downgrade_preflight_blocks_clone_and_stale_clock_hazards_without_mutation() {
        let f = Fixture::new();
        let path = f.0.join("accounts.json");
        let value = json!({"version":3,"active_account_id":null,"accounts":[{
            "id":"synthetic","name":"fixture","email":null,"plan_type":null,"last_used_at":null,
            "options":{"account_link":"manual:fixture","appearance_updated_at":{"color":"2026-10-24T00:00:00Z"}}
        }]});
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            legacy_downgrade_blockers(&f.0).unwrap(),
            [
                "profile_manual_link_metadata",
                "profile_appearance_clock_metadata"
            ]
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::write(&path, b"not valid account metadata").unwrap();
        assert!(legacy_downgrade_blockers(&f.0).is_err());
        fs::write(
            &path,
            br#"{"version":3,"accounts":[{"options":false}],"active_account_id":null}"#,
        )
        .unwrap();
        assert!(legacy_downgrade_blockers(&f.0).is_err());
        assert!(
            !f.0.join("runtime/profile-link").exists(),
            "preflight must not seed a secret or tombstone"
        );
    }
}
