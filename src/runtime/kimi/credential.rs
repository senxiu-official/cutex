//! A Kimi agent's launch credential (r266).
//!
//! Profile launches go through the Profile resolver (K2,
//! `profiles::kimi_launch::resolve`) with the agent's frozen binding at every
//! start: a stored Kimi / Moonshot API key (a new key applies to runtimes
//! started afterwards) or a Kimi account login (the agent's home links the
//! account's `credentials/` and `oauth/`, so every agent of the login shares
//! one token file and Kimi's own refresh lock). An `env` source reads the named
//! daemon variable (unmanaged hosts, offline tests). The key reaches only the
//! adapter's environment and, mapped to `KIMI_PROFILE_API_KEY`, the Kimi child;
//! config.toml names the variable, never the value.

use std::path::Path;

use anyhow::{ensure, Context};

use super::{KimiCredentialSource, KimiOptions, API_KEY_PROVIDER, KIMI_KEY_ENV};
use crate::profiles::kimi_launch::KimiLaunch;
use crate::runtime::dsh::DshSession;
use crate::session::model::CutexSessionRecord;

pub struct ResolvedCredential {
    /// `[providers.<id>]` the agent's models use.
    pub provider_id: String,
    /// Top-level tables for config.toml (`providers`, and for an account also
    /// its `models`, `services`, `default_model`).
    pub config: toml::Table,
    /// `(adapter env name, value)`; the adapter maps it to `KIMI_PROFILE_API_KEY`.
    pub secret_env: Option<(String, String)>,
    /// API-key launches: cutex declares the model aliases (`[models.<model>]`).
    pub declare_models: bool,
    /// The provider's endpoint as configured (before a private-test override).
    pub base_url: Option<String>,
    /// Profile API-key launches: the model family by provider id.
    pub model_family: Option<crate::profiles::kimi::ModelFamily>,
    launch: Option<KimiLaunch>,
}

impl ResolvedCredential {
    pub fn install_into(&self, home: &Path) -> anyhow::Result<()> {
        match &self.launch {
            Some(launch) => launch.install_into(home),
            None => Ok(()),
        }
    }

    /// The Kimi account of an account launch.
    pub fn account(&self) -> Option<super::account_tokens::AccountRef> {
        match &self.launch.as_ref()?.kind {
            crate::profiles::kimi_launch::KimiCredentialKind::Account { account_id, home, slot } => {
                Some(super::account_tokens::AccountRef { account_id: account_id.clone(), home: home.clone(), slot: slot.clone() })
            }
            crate::profiles::kimi_launch::KimiCredentialKind::ApiKey { .. } => None,
        }
    }
}

/// The Kimi account a record is bound to (its frozen Profile binding), without
/// resolving the launch: also for offline reads of an agent whose account is
/// logged out.
pub fn bound_account(record: &CutexSessionRecord) -> Option<super::account_tokens::AccountRef> {
    let id = record.profile_binding.as_ref()?.credential.kimi_account_id.clone()?;
    let home = crate::profiles::kimi_accounts::account_home(&id).ok()?;
    let slot = crate::profiles::kimi_accounts::home_slot(&home);
    Some(super::account_tokens::AccountRef { account_id: id, home, slot })
}

fn api_key_provider(base_url: &str) -> toml::Table {
    let provider: toml::Table = [
        ("type".to_string(), toml::Value::String("kimi".into())),
        ("base_url".to_string(), toml::Value::String(base_url.into())),
        ("api_key_env".to_string(), toml::Value::String(KIMI_KEY_ENV.into())),
    ]
    .into_iter()
    .collect();
    let providers: toml::Table = [(API_KEY_PROVIDER.to_string(), toml::Value::Table(provider))].into_iter().collect();
    [("providers".to_string(), toml::Value::Table(providers))].into_iter().collect()
}

pub fn resolve(record: &CutexSessionRecord, _session: &DshSession, options: &KimiOptions) -> anyhow::Result<ResolvedCredential> {
    match &options.credential {
        KimiCredentialSource::Env { env_name, base_url } => {
            ensure!(record.profile_binding.is_none(), "this Kimi agent has a Profile binding but an env credential; create a new agent");
            let value = std::env::var(env_name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .with_context(|| format!("credential_missing: Kimi requires the {env_name} environment variable on the daemon host"))?;
            Ok(ResolvedCredential {
                provider_id: API_KEY_PROVIDER.into(),
                config: api_key_provider(base_url),
                secret_env: Some((env_name.clone(), value)),
                declare_models: true,
                base_url: Some(base_url.clone()),
                model_family: None,
                launch: None,
            })
        }
        KimiCredentialSource::Profile => {
            let binding = record.profile_binding.as_ref().context("invalid_binding: this Kimi agent's Profile binding is missing; create a new agent")?;
            let launch = crate::profiles::kimi_launch::resolve(binding).map_err(anyhow::Error::new)?;
            let declare_models = launch.secret_env.is_some();
            let mut config = launch.config.clone();
            let base_url = provider_base_url(&config, &launch.provider_id);
            if declare_models {
                // Private debug tests only (a marked test home): the offline
                // provider instead of Kimi's official host.
                if let Some(url) = crate::profiles::kimi::test_override(crate::profiles::kimi::TEST_BASE_URL_ENV) {
                    if let Some(provider) = config.get_mut("providers").and_then(|providers| providers.get_mut(&launch.provider_id)).and_then(toml::Value::as_table_mut) {
                        provider.insert("base_url".into(), toml::Value::String(url));
                    }
                }
            }
            Ok(ResolvedCredential {
                provider_id: launch.provider_id.clone(),
                config,
                secret_env: launch.secret_env.as_ref().map(|(name, key)| (name.clone(), key.expose().to_string())),
                declare_models,
                base_url,
                model_family: launch.model_family,
                launch: Some(launch),
            })
        }
    }
}

fn provider_base_url(config: &toml::Table, provider: &str) -> Option<String> {
    config.get("providers")?.get(provider)?.get("base_url")?.as_str().map(str::to_string)
}

/// The key this agent's next start would use, for redaction only.
pub fn for_redaction(record: &CutexSessionRecord) -> Option<String> {
    let session = record.dsh_session.as_ref()?;
    let options = session.kimi.as_ref()?;
    resolve(record, session, options).ok()?.secret_env.map(|(_, value)| value).filter(|value| !value.trim().is_empty())
}
