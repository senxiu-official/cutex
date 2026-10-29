//! r266 Kimi launch credential resolver (docs/development/r266-kimi-profiles.md §5).
//!
//! K1 (kimi-runtime) calls [`resolve`] with a Kimi session's frozen launch
//! binding before every start and gets the provider tables for the agent's
//! `config.toml`, the secret env (API-key launches) and, for Kimi accounts,
//! [`KimiLaunch::install_into`] which links the agent's `credentials/` and
//! `oauth/` to the account home (§4: one credential file, one refresh lock).

use std::path::{Path, PathBuf};

use serde_json::json;

use super::runtime_variants::{ProfileLaunchBinding, Source};
use super::stored_keys::SecretKey;

/// Carrier variable of a stored API key for the kimi process, also the
/// provider's `api_key_env` in the generated config.
pub const KIMI_STORED_KEY_ENV: &str = "KIMI_PROFILE_API_KEY";
/// Provider id of API-key launches in the agent config.
pub const API_KEY_PROVIDER_ID: &str = "cutex-kimi";

#[derive(Clone, PartialEq, Eq)]
pub enum KimiCredentialKind {
    ApiKey { profile_id: String, version: Option<u64> },
    Account { account_id: String, home: PathBuf, slot: String },
}

impl std::fmt::Debug for KimiCredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { profile_id, version } => write!(f, "ApiKey {{ profile_id: {profile_id:?}, version: {version:?} }}"),
            Self::Account { account_id, home, slot } => write!(f, "Account {{ account_id: {account_id:?}, home: {home:?}, slot: {slot:?} }}"),
        }
    }
}

/// What a Kimi start needs. Never stored in `launch.json`; `Debug` redacts.
#[derive(Clone)]
pub struct KimiLaunch {
    pub kind: KimiCredentialKind,
    /// `(name, value)` for the kimi process only (API-key launches).
    pub secret_env: Option<(String, SecretKey)>,
    /// Provider id the generated config selects (`[providers.<id>]`).
    pub provider_id: String,
    /// Top-level tables to merge into the agent's `config.toml`:
    /// `providers.<id>` always; Kimi accounts also `models.*`, `services.*`
    /// and `default_model` copied from the account config (non-secret).
    pub config: toml::Table,
    /// Model alias to start with when the binding has none.
    pub default_model: Option<String>,
    /// API-key launches: Kimi's documented model family by the profile's
    /// provider id (never by URL text; `None`: provider `kimi` / accounts).
    pub model_family: Option<super::kimi::ModelFamily>,
}

impl std::fmt::Debug for KimiLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KimiLaunch")
            .field("kind", &self.kind)
            .field("secret_env", &self.secret_env.as_ref().map(|(name, _)| (name.as_str(), "<redacted>")))
            .field("provider_id", &self.provider_id)
            .field("default_model", &self.default_model)
            .finish_non_exhaustive()
    }
}

/// Refusal of a Kimi start; `code()` is the Management error code. Messages
/// never contain a key or token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiLaunchError {
    pub code: &'static str,
    pub message: String,
}

impl KimiLaunchError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn code(&self) -> &'static str {
        self.code
    }
    pub fn details(&self) -> serde_json::Value {
        json!({ "code": self.code })
    }
}

impl std::fmt::Display for KimiLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for KimiLaunchError {}

fn table(entries: impl IntoIterator<Item = (&'static str, toml::Value)>) -> toml::Table {
    entries.into_iter().map(|(key, value)| (key.to_string(), value)).collect()
}

/// Resolve a Kimi session's frozen binding (see the module docs).
pub fn resolve(binding: &ProfileLaunchBinding) -> Result<KimiLaunch, KimiLaunchError> {
    if binding.runtime != super::kimi::RUNTIME {
        return Err(KimiLaunchError::new("invalid_binding", format!("binding runtime {} is not kimi", binding.runtime)));
    }
    match binding.credential.source.as_str() {
        source if source == Source::Stored.as_str() => resolve_api_key(binding),
        source if source == Source::KimiAccount.as_str() => {
            let id = binding
                .credential
                .kimi_account_id
                .as_deref()
                .ok_or_else(|| KimiLaunchError::new("invalid_binding", "Kimi account binding without an account id"))?;
            resolve_account(id)
        }
        other => Err(KimiLaunchError::new("invalid_binding", format!("Kimi credential source {other:?} is not supported by this cutex"))),
    }
}

fn resolve_api_key(binding: &ProfileLaunchBinding) -> Result<KimiLaunch, KimiLaunchError> {
    let store = super::settings_api::load_store()
        .map_err(|_| KimiLaunchError::new("credential_missing", "the profile store could not be read"))?;
    let account = store.accounts.iter().find(|account| account.id == binding.profile_id).ok_or_else(|| {
        KimiLaunchError::new("credential_missing", format!("the profile {} this agent was created on no longer exists", binding.profile_id))
    })?;
    if !super::kimi::is_native_api_key(account) {
        return Err(KimiLaunchError::new("variant_runtime_unavailable", format!("profile {} is no longer a Kimi / Moonshot API-key profile", account.name)));
    }
    // profile-relay-runtimes (B3): the profile's own address, official or a
    // relay; never guessed and never replaced by Kimi's official host.
    let base_url = match super::interface_urls::require(account, super::interface_urls::InterfaceKind::KimiNative) {
        Ok(Some(url)) => url,
        Ok(None) | Err(super::interface_urls::BASE_URL_MISSING) => {
            return Err(KimiLaunchError::new(super::interface_urls::BASE_URL_MISSING, format!("profile {} has no base URL for Kimi Code", account.name)))
        }
        Err(code) => {
            return Err(KimiLaunchError::new(code, format!("Kimi Code on this host cannot use profile {}'s base URL", account.name)))
        }
    };
    let key = super::stored_keys::read_for_account(account).map_err(|_| {
        KimiLaunchError::new("credential_missing", format!("profile {} has no stored API key; set one in the profile settings", account.name))
    })?;
    let provider = table([
        ("type", toml::Value::String("kimi".into())),
        ("base_url", toml::Value::String(base_url)),
        ("api_key_env", toml::Value::String(KIMI_STORED_KEY_ENV.into())),
    ]);
    let mut providers = toml::Table::new();
    providers.insert(API_KEY_PROVIDER_ID.into(), toml::Value::Table(provider));
    let config = table([("providers", toml::Value::Table(providers))]);
    Ok(KimiLaunch {
        kind: KimiCredentialKind::ApiKey { profile_id: account.id.clone(), version: binding.credential.version },
        secret_env: Some((KIMI_STORED_KEY_ENV.to_string(), key)),
        provider_id: API_KEY_PROVIDER_ID.into(),
        config,
        default_model: super::kimi::api_key_default_model(account),
        model_family: super::kimi::account_model_family(account),
    })
}

fn resolve_account(id: &str) -> Result<KimiLaunch, KimiLaunchError> {
    let registry = super::kimi_accounts::load()
        .map_err(|_| KimiLaunchError::new("kimi_account_not_found", "the Kimi accounts registry could not be read"))?;
    if registry.find(id).is_none() {
        return Err(KimiLaunchError::new("kimi_account_not_found", format!("the Kimi account {id} this agent was created on no longer exists")));
    }
    let home = super::kimi_accounts::account_home(id).map_err(|_| KimiLaunchError::new("kimi_account_not_found", "invalid Kimi account id"))?;
    let account_config = super::kimi::read_config(&home)
        .map_err(|_| KimiLaunchError::new("credential_missing", format!("the Kimi account {id} has an unreadable config; log in again")))?;
    let slot = super::kimi_accounts::slot_from_config(&account_config).unwrap_or_else(|| super::kimi_accounts::DEFAULT_SLOT.into());
    let state = super::kimi_accounts::credential_path(&home, &slot)
        .map(|path| super::kimi_accounts::read_credential_state(&path))
        .unwrap_or(super::kimi_accounts::CredentialState::Missing);
    match state {
        super::kimi_accounts::CredentialState::Valid { .. } => {}
        super::kimi_accounts::CredentialState::Revoked => {
            return Err(KimiLaunchError::new("kimi_account_logged_out", format!("the Kimi account {id} is logged out; log in again")))
        }
        super::kimi_accounts::CredentialState::Missing => {
            return Err(KimiLaunchError::new("credential_missing", format!("the Kimi account {id} has never logged in")))
        }
    }
    let provider = account_config
        .get("providers")
        .and_then(|providers| providers.get(super::kimi::MANAGED_PROVIDER))
        .cloned()
        .ok_or_else(|| KimiLaunchError::new("credential_missing", format!("the Kimi account {id} has no provisioned provider; log in again")))?;
    let mut providers = toml::Table::new();
    providers.insert(super::kimi::MANAGED_PROVIDER.into(), provider);
    let mut config = table([("providers", toml::Value::Table(providers))]);
    if let Some(models) = account_config.get("models").and_then(toml::Value::as_table) {
        let managed: toml::Table = models
            .iter()
            .filter(|(_, entry)| entry.get("provider").and_then(toml::Value::as_str) == Some(super::kimi::MANAGED_PROVIDER))
            .map(|(alias, entry)| (alias.clone(), entry.clone()))
            .collect();
        if !managed.is_empty() {
            config.insert("models".into(), toml::Value::Table(managed));
        }
    }
    if let Some(services) = account_config.get("services").and_then(toml::Value::as_table) {
        let kept: toml::Table = services
            .iter()
            .filter(|(name, _)| matches!(name.as_str(), "moonshot_search" | "moonshot_fetch"))
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect();
        if !kept.is_empty() {
            config.insert("services".into(), toml::Value::Table(kept));
        }
    }
    let catalog = super::kimi::catalog_from_config(&account_config);
    if let Some(default) = &catalog.default_model {
        config.insert("default_model".into(), toml::Value::String(default.clone()));
    }
    Ok(KimiLaunch {
        kind: KimiCredentialKind::Account { account_id: id.to_string(), home, slot },
        secret_env: None,
        provider_id: super::kimi::MANAGED_PROVIDER.into(),
        config,
        default_model: catalog.default_model,
        model_family: None,
    })
}

impl KimiLaunch {
    /// Values the adapter must redact from anything it records.
    pub fn redaction_values(&self) -> Vec<String> {
        self.secret_env.iter().map(|(_, key)| key.expose().to_string()).collect()
    }

    /// Prepare the agent's `KIMI_CODE_HOME` (idempotent): Kimi accounts link
    /// `credentials/` and `oauth/` to the account home and copy `device_id`;
    /// API-key launches remove links a home must not keep.
    pub fn install_into(&self, kimi_code_home: &Path) -> anyhow::Result<()> {
        crate::management::client_tokens::ensure_private_dir(kimi_code_home)?;
        match &self.kind {
            KimiCredentialKind::Account { home, .. } => {
                for name in ["credentials", "oauth"] {
                    let target = home.join(name);
                    crate::management::client_tokens::ensure_private_dir(&target)?;
                    link_dir(&target, &kimi_code_home.join(name))?;
                }
                if let Ok(device) = std::fs::read(home.join("device_id")) {
                    crate::config::atomic::write_private_bytes_atomic(&kimi_code_home.join("device_id"), &device)?;
                }
            }
            KimiCredentialKind::ApiKey { .. } => {
                for name in ["credentials", "oauth"] {
                    let path = kimi_code_home.join(name);
                    if is_link(&path) {
                        remove_link(&path)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        if metadata.file_type().is_symlink() {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        }
        #[allow(unreachable_code)]
        false
    })
}

fn remove_link(path: &Path) -> anyhow::Result<()> {
    // A symlink is removed as a file on Unix; a junction as a directory on Windows.
    std::fs::remove_file(path)
        .or_else(|_| std::fs::remove_dir(path))
        .map_err(|error| anyhow::anyhow!("remove the Kimi link {}: {}", path.display(), error.kind()))
}

fn points_to(link: &Path, target: &Path) -> bool {
    match (std::fs::canonicalize(link), std::fs::canonicalize(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `link` → `target` as a directory link (symlink; junction on Windows).
/// An existing real directory (never written by cutex) is moved aside.
pub fn link_dir(target: &Path, link: &Path) -> anyhow::Result<()> {
    if is_link(link) {
        if points_to(link, target) {
            return Ok(());
        }
        remove_link(link)?;
    } else if link.exists() {
        let aside = link.with_file_name(format!(
            "{}.replaced-{}",
            link.file_name().and_then(|name| name.to_str()).unwrap_or("dir"),
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.f")
        ));
        std::fs::rename(link, &aside).map_err(|error| anyhow::anyhow!("move aside {}: {}", link.display(), error.kind()))?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).map_err(|error| anyhow::anyhow!("link {}: {}", link.display(), error.kind()))?;
    #[cfg(windows)]
    {
        // `mklink /J` refuses a path that mixes `/` and `\` separators.
        let native = |path: &Path| PathBuf::from(path.as_os_str().to_string_lossy().replace('/', "\\"));
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(native(link))
            .arg(native(target))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(status.success(), "create the Kimi credentials junction {}", link.display());
    }
    Ok(())
}

#[cfg(test)]
#[path = "kimi_launch_tests.rs"]
mod tests;
