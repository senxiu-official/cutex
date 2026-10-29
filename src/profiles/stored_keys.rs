//! r265 stored Profile keys (docs/development/r265-profile-variants.md §3).
//!
//! DSH profiles keep their write-only key in `<config>/profile-keys/<id>.json`
//! (0600 in a 0700 directory, the same private writer as Claude account keys).
//! Codex API-key profiles keep theirs in the profile `auth.json` as before;
//! [`read_for_profile`] reads either for the runtime that is being started.
//! Nothing here formats a key into an error, a log line or a `Debug` value.

use std::path::PathBuf;

use anyhow::{bail, ensure, Context};
use serde_json::{json, Value};

use super::model::{CliKind, StoredAccount};

pub const KEYS_DIR: &str = "profile-keys";
/// Adapter-process variable that carries a stored key to the DSH adapter,
/// which maps it to `DEEPSEEK_API_KEY` for the DSH child only. Never an
/// owner-chosen env source (see [`validate_env_source_name`]).
pub const DSH_STORED_KEY_ENV: &str = "DSH_PROFILE_API_KEY";
pub const MAX_KEY_BYTES: usize = 4096;

/// A secret that never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey(String);

impl SecretKey {
    pub fn new(raw: &str) -> anyhow::Result<Self> {
        let key = raw.trim();
        ensure!(
            !key.is_empty() && key.len() <= MAX_KEY_BYTES && !key.chars().any(|c| c.is_control() || c.is_whitespace()),
            "API key must be 1-{MAX_KEY_BYTES} non-whitespace characters"
        );
        Ok(Self(key.to_string()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

fn validate_profile_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id != "."
            && id != ".."
            && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "invalid profile id for a stored key"
    );
    Ok(())
}

pub fn keys_root() -> anyhow::Result<PathBuf> {
    Ok(crate::config::paths::config_dir()?.join(KEYS_DIR))
}

pub fn key_path(profile_id: &str) -> anyhow::Result<PathBuf> {
    validate_profile_id(profile_id)?;
    Ok(keys_root()?.join(format!("{profile_id}.json")))
}

/// Earlier keys kept for redaction only (newest last), per profile.
pub const MAX_RETIRED_KEYS: usize = 8;

/// The key file: `{ apiKey?, version, retired: [{ version, apiKey }] }`.
/// One atomic write carries the key AND its version, so a crash between it
/// and the profile store save can never pair a key with another version
/// ([`stored_version`] is the authority for a stored key). `retired` holds
/// replaced / removed keys of this profile so the native logs written while
/// they were current stay redacted when they are projected again (revert,
/// fork, offline history, a runtime catching up); it is never read back
/// through any API.
fn read_json(profile_id: &str) -> anyhow::Result<Option<Value>> {
    let path = key_path(profile_id)?;
    match std::fs::read(&path) {
        Ok(raw) => serde_json::from_slice(&raw).map(Some).map_err(|_| anyhow::anyhow!("stored profile key file is not valid JSON")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => bail!("read stored profile key: {}", error.kind()),
    }
}

fn retired_with(previous: Option<&Value>, replaced: Option<(&str, u64)>) -> Vec<Value> {
    let mut retired: Vec<Value> = previous
        .and_then(|file| file.get("retired"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some((key, version)) = replaced {
        retired.retain(|entry| entry.get("apiKey").and_then(Value::as_str) != Some(key));
        retired.push(json!({ "version": version, "apiKey": key }));
    }
    let excess = retired.len().saturating_sub(MAX_RETIRED_KEYS);
    retired.drain(..excess);
    retired
}

fn write_file(profile_id: &str, value: &Value) -> anyhow::Result<()> {
    let root = keys_root()?;
    crate::management::client_tokens::ensure_private_dir(&root)?;
    crate::config::atomic::write_private_pretty_json_atomic(&key_path(profile_id)?, value, "profile API key")
}

fn current_of(file: Option<&Value>) -> Option<(&str, u64)> {
    let file = file?;
    let key = file.get("apiKey").and_then(Value::as_str)?;
    Some((key, file.get("version").and_then(Value::as_u64).unwrap_or(0)))
}

/// Write (replace) the stored key of a DSH profile. Returns nothing about the key.
pub fn write(profile_id: &str, key: &SecretKey, version: u64) -> anyhow::Result<()> {
    let previous = read_json(profile_id)?;
    let replaced = current_of(previous.as_ref()).filter(|(old, _)| *old != key.expose());
    let retired = retired_with(previous.as_ref(), replaced);
    write_file(profile_id, &json!({ "apiKey": key.expose(), "version": version, "retired": retired }))
}

/// Remove the stored key (`apiKey: null`). Its value stays among the
/// retired keys (redaction only); `version` records the removal. Returns
/// whether a key was present.
pub fn remove(profile_id: &str, version: u64) -> anyhow::Result<bool> {
    let previous = read_json(profile_id)?;
    let Some(current) = current_of(previous.as_ref()) else { return Ok(false) };
    let retired = retired_with(previous.as_ref(), Some(current));
    write_file(profile_id, &json!({ "version": version, "retired": retired }))?;
    Ok(true)
}

/// Remember a key of the profile that is being replaced elsewhere (Codex
/// API-key profiles keep the current key in their `auth.json`): redaction only.
pub fn retire(profile_id: &str, key: &str, version: u64) -> anyhow::Result<()> {
    if key.trim().is_empty() {
        return Ok(());
    }
    let mut file = read_json(profile_id)?.unwrap_or_else(|| json!({}));
    let retired = retired_with(Some(&file), Some((key, version)));
    file["retired"] = json!(retired);
    write_file(profile_id, &file)
}

/// The profile is gone: its key file goes too (current and retired keys).
pub fn purge(profile_id: &str) -> anyhow::Result<bool> {
    let path = key_path(profile_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(anyhow::anyhow!("remove stored profile key: {}", error.kind())),
    }
}

/// The version recorded WITH the stored key (atomically, in its file), if any.
pub fn stored_version(profile_id: &str) -> Option<u64> {
    read_json(profile_id).ok().flatten()?.get("version").and_then(Value::as_u64)
}

/// The raw key file (rollback of a failed profile save restores it as it was).
pub(crate) fn snapshot(profile_id: &str) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(key_path(profile_id)?) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => bail!("read stored profile key: {}", error.kind()),
    }
}

pub(crate) fn restore(profile_id: &str, snapshot: Option<&[u8]>) -> anyhow::Result<()> {
    match snapshot {
        Some(raw) => crate::config::atomic::write_private_bytes_atomic(&key_path(profile_id)?, raw),
        None => purge(profile_id).map(|_| ()),
    }
}

/// Every value of this profile's key that runtime output must never show:
/// the retired keys (stored file) plus, by the caller, the current one.
pub fn retired_values(profile_id: &str) -> Vec<String> {
    read_json(profile_id)
        .ok()
        .flatten()
        .and_then(|file| file.get("retired").cloned())
        .and_then(|retired| retired.as_array().cloned())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("apiKey").and_then(Value::as_str).map(str::to_owned))
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn read_file(profile_id: &str) -> anyhow::Result<Option<SecretKey>> {
    let Some(value) = read_json(profile_id)? else { return Ok(None) };
    match value.get("apiKey").and_then(Value::as_str) {
        Some(key) => SecretKey::new(key).map(Some).map_err(|_| anyhow::anyhow!("stored profile key file holds an invalid key")),
        None => Ok(None),
    }
}

pub fn present(profile_id: &str) -> bool {
    read_file(profile_id).ok().flatten().is_some()
}

/// The profile's own stored key, as the runtime being started needs it:
/// DSH profiles from the private key file, Codex API-key profiles from the
/// profile `auth.json`. Other profiles have no stored key.
pub fn read_for_account(account: &StoredAccount) -> anyhow::Result<SecretKey> {
    let found = match account.cli_kind {
        CliKind::Dsh => read_file(&account.id)?,
        CliKind::Codex if super::runtime_variants::is_api_key_profile(account) => {
            let files = super::materialize::materialized_account_files(account)?;
            crate::launch::env::codex_api_key_from_auth_file(&files.auth_path)
                .map(|key| SecretKey::new(&key))
                .transpose()
                .map_err(|_| anyhow::anyhow!("profile auth file holds an invalid API key"))?
        }
        _ => bail!("profile {} has no stored API key credential", account.name),
    };
    found.with_context(|| format!("credential_missing: profile {} has no stored API key; set one in the profile settings", account.name))
}

/// By profile ID (launch bindings keep the ID; names can change).
pub fn read_for_profile(profile_id: &str) -> anyhow::Result<SecretKey> {
    let store = super::settings_api::load_store()?;
    let account = store
        .accounts
        .iter()
        .find(|account| account.id == profile_id)
        .with_context(|| format!("the profile {profile_id} this agent was created on no longer exists; its stored key is gone"))?;
    read_for_account(account)
}

/// An owner-chosen env source name: the DSH rules plus the stored-key carrier.
pub fn validate_env_source_name(name: &str) -> anyhow::Result<()> {
    ensure!(
        !name.eq_ignore_ascii_case(DSH_STORED_KEY_ENV) && !name.eq_ignore_ascii_case(super::kimi_launch::KIMI_STORED_KEY_ENV),
        "envName {name} is reserved for stored keys"
    );
    super::dsh::DshOptions { api_key_env: name.to_string(), ..Default::default() }.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_key_file_is_private_and_never_debug_printed() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let key = SecretKey::new("  sk-fake-r265-stored  ").unwrap();
        assert!(!format!("{key:?}").contains("sk-fake"));
        assert!(!present("p1"));
        write("p1", &key, 1).unwrap();
        assert!(present("p1"));
        assert_eq!(read_file("p1").unwrap().unwrap().expose(), "sk-fake-r265-stored");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(key_path("p1").unwrap()).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "key file must be owner-only");
            let dir = std::fs::metadata(keys_root().unwrap()).unwrap().permissions().mode();
            assert_eq!(dir & 0o077, 0, "key dir must be owner-only");
        }
        assert!(remove("p1", 2).unwrap());
        assert!(!remove("p1", 3).unwrap());
        assert!(!present("p1"));
        assert!(purge("p1").unwrap());
        for bad in ["", "..", "a/b", "a\\b", "x y"] {
            assert!(key_path(bad).is_err());
        }
        for bad in ["", "has space", "a\nb"] {
            let error = SecretKey::new(bad).unwrap_err().to_string();
            assert!(!error.contains(bad) || bad.is_empty());
        }
        assert!(validate_env_source_name("DSH_PROFILE_API_KEY").is_err());
        assert!(validate_env_source_name("MY_DEEPSEEK_KEY").is_ok());
        assert!(validate_env_source_name("CUTEX_X").is_err());
    }

    /// Rotation and removal keep earlier values for redaction (bounded),
    /// in the same atomic write as the new key and its version.
    #[test]
    fn rotation_keeps_retired_keys_for_redaction_and_the_version_with_the_key() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let key = |raw: &str| SecretKey::new(raw).unwrap();
        write("p2", &key("sk-fake-r265-a"), 1).unwrap();
        assert!(retired_values("p2").is_empty());
        write("p2", &key("sk-fake-r265-b"), 2).unwrap();
        write("p2", &key("sk-fake-r265-b"), 3).unwrap();
        assert_eq!(retired_values("p2"), ["sk-fake-r265-a"], "rewriting the same key retires nothing");
        assert_eq!(stored_version("p2"), Some(3));
        assert!(remove("p2", 4).unwrap());
        assert_eq!(retired_values("p2"), ["sk-fake-r265-a", "sk-fake-r265-b"]);
        assert_eq!(stored_version("p2"), Some(4));
        assert!(!present("p2"));
        for n in 0..12 {
            write("p2", &key(&format!("sk-fake-r265-n{n}")), 5 + n).unwrap();
        }
        let retired = retired_values("p2");
        assert_eq!(retired.len(), MAX_RETIRED_KEYS);
        assert_eq!(retired.last().unwrap(), "sk-fake-r265-n10");
        retire("codex-profile", "sk-fake-r265-codex-old", 1).unwrap();
        assert_eq!(retired_values("codex-profile"), ["sk-fake-r265-codex-old"]);
        assert!(!present("codex-profile"), "a retired-only file holds no current key");
        assert!(purge("p2").unwrap());
        assert!(retired_values("p2").is_empty());
    }
}
