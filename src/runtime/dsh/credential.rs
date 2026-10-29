//! DSH secret-env resolution (r265, workstream C3; docs/development/r265-profile-variants.md §3, §7).
//!
//! The key reaches only the adapter process environment, under the name the
//! launch file's `secretEnvMappings` maps to `DEEPSEEK_API_KEY` for the DSH
//! child; `launch.json` never holds the value. The source is the session's
//! frozen binding: a stored key is read fresh at every start (rotation applies
//! to runtimes started afterwards) and never falls back to a host variable,
//! and an env source never falls back to a stored key.

use anyhow::{bail, ensure, Context};

use super::DshSession;
use crate::profiles::stored_keys::DSH_STORED_KEY_ENV;
use crate::session::model::CutexSessionRecord;

/// `(adapter env name, value)` for a product (non-spike) DSH start.
pub(super) fn secret_env(record: &CutexSessionRecord, session: &DshSession) -> anyhow::Result<(String, String)> {
    let binding = record.profile_binding.as_ref();
    match binding.map(|binding| binding.credential.source.as_str()) {
        Some("stored") => {
            let binding = binding.expect("matched a binding");
            ensure!(
                session.api_key_env == DSH_STORED_KEY_ENV,
                "DSH session binding and launch record disagree about the credential source"
            );
            let key = crate::profiles::stored_keys::read_for_profile(&binding.profile_id)?;
            Ok((DSH_STORED_KEY_ENV.to_string(), key.expose().to_string()))
        }
        Some("env") | None => {
            ensure!(
                session.api_key_env != DSH_STORED_KEY_ENV,
                "this DSH agent was created with a stored profile key but its launch binding is missing; create a new agent"
            );
            if let Some(name) = binding.and_then(|binding| binding.credential.env_name.as_deref()) {
                ensure!(name == session.api_key_env, "DSH session binding and launch record disagree about the key variable");
            }
            let value = std::env::var(&session.api_key_env)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .with_context(|| format!("DSH requires the {} environment variable on the daemon host", session.api_key_env))?;
            Ok((session.api_key_env.clone(), value))
        }
        Some(other) => bail!("DSH credential source {other:?} is not supported by this cutex"),
    }
}

/// DSH's endpoint variables (`@deepseek-ai/dsh-llm-deepseek` reads
/// `DEEPSEEK_BASE_URL`, `dsh-web-search-deepseek` `DEEPSEEK_SEARCH_BASE_URL`;
/// the inherited process layer wins over project / DSH-home `.env` files).
pub const BASE_URL_ENV: &str = "DEEPSEEK_BASE_URL";
pub const SEARCH_BASE_URL_ENV: &str = "DEEPSEEK_SEARCH_BASE_URL";

/// profile-relay-runtimes (B4): the non-secret endpoint variables of a
/// product DSH start, from the bound profile's current `deepseekNative`
/// address (official or a relay; read at every start like the key, so an
/// address change applies to runtimes started afterwards). Refuses with
/// `variant_base_url_missing` when the profile's main address is a relay and
/// no DSH address is set: a relay key never goes to DeepSeek's host.
/// Sessions without a binding (pre-r265) or whose profile is gone keep DSH's
/// built-in official default (no variables), as before.
pub(super) fn endpoint_env(record: &CutexSessionRecord) -> anyhow::Result<Vec<(String, String)>> {
    use crate::profiles::interface_urls::{require, InterfaceKind};
    let Some(binding) = record.profile_binding.as_ref() else { return Ok(Vec::new()) };
    let store = crate::profiles::settings_api::load_store()?;
    let Some(account) = store.accounts.iter().find(|account| account.id == binding.profile_id) else {
        return Ok(Vec::new());
    };
    match require(account, InterfaceKind::DeepseekNative) {
        Ok(Some(url)) => {
            let search = format!("{}/v1", url.trim_end_matches('/'));
            Ok(vec![(BASE_URL_ENV.to_string(), url), (SEARCH_BASE_URL_ENV.to_string(), search)])
        }
        Ok(None) => Ok(Vec::new()),
        Err(code) => bail!(
            "{code}: profile {} uses a relay base URL and has no DSH (DeepSeek Anthropic-compatible) address; set it in the profile settings",
            account.name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::runtime_variants::{BoundCredential, ProfileLaunchBinding};

    fn session(env: &str) -> DshSession {
        DshSession {
            flavor: Default::default(), kimi: None,
            thread_id: "t".into(), state_dir: "/x".into(), node: "/n".into(), adapter: "/a".into(), dsh_cli: "/d".into(),
            patch: "/x/profile.patch.yml".into(), install_root: "/i".into(), native_session_id: None,
            deployment_version: "v".into(), model: "deepseek-v4-flash".into(), reasoning_effort: None,
            api_key_env: env.into(), spike: false,
        }
    }

    fn record(binding: Option<ProfileLaunchBinding>) -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new("cutex.test".into(), None, "h".into(), "/tmp".into(), None).unwrap();
        record.profile_binding = binding;
        record
    }

    fn binding(profile_id: &str, source: &str, env_name: Option<&str>) -> ProfileLaunchBinding {
        ProfileLaunchBinding {
            profile_id: profile_id.into(), variant_id: "dsh".into(), runtime: "dsh".into(),
            credential: BoundCredential { source: source.into(), env_name: env_name.map(Into::into), version: None, kimi_account_id: None },
            model: None, reasoning_effort: None, runtime_package_version: None, bound_at: "now".into(),
        }
    }

    /// profile-relay-runtimes (B4): the launch env carries the bound
    /// profile's DSH address; a relay main address without one refuses.
    #[test]
    fn endpoint_env_carries_the_profile_dsh_address() {
        use crate::profiles::interface_urls::{BASE_URL_MISSING, DEEPSEEK_ANTHROPIC_BASE_URL};
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let create = |body: &str| -> String {
            crate::management::v2::profiles_api::create_from_body(body.as_bytes()).unwrap()["profile"]["id"].as_str().unwrap().to_string()
        };
        let env = |id: &str| endpoint_env(&record(Some(binding(id, "stored", None))));
        let official = create(r#"{"name":"DS","preset":"deepseek","apiKey":"sk-fake-r267-a"}"#);
        assert_eq!(env(&official).unwrap(), [
            (BASE_URL_ENV.to_string(), DEEPSEEK_ANTHROPIC_BASE_URL.to_string()),
            (SEARCH_BASE_URL_ENV.to_string(), format!("{DEEPSEEK_ANTHROPIC_BASE_URL}/v1")),
        ]);
        let dsh = create(r#"{"name":"DSH relay","kind":"dsh","interfaceBaseUrls":{"deepseekNative":"http://127.0.0.1:9/anthropic/"}}"#);
        assert_eq!(env(&dsh).unwrap(), [
            (BASE_URL_ENV.to_string(), "http://127.0.0.1:9/anthropic/".to_string()),
            (SEARCH_BASE_URL_ENV.to_string(), "http://127.0.0.1:9/anthropic/v1".to_string()),
        ]);
        let relay = create(r#"{"name":"DS relay","preset":"deepseek","baseUrl":"https://relay.example/v1","apiKey":"sk-fake-r267-b"}"#);
        let error = format!("{:#}", env(&relay).unwrap_err());
        assert!(error.starts_with(BASE_URL_MISSING) && !error.contains("sk-fake"), "{error}");
        let patch = crate::management::v2::profiles_api::parse_patch(
            br#"{"interfaceBaseUrls":{"deepseekNative":"https://relay.example/anthropic"}}"#).unwrap();
        crate::profiles::settings_api::update_profile(&relay, &patch).unwrap();
        assert_eq!(env(&relay).unwrap()[0], (BASE_URL_ENV.to_string(), "https://relay.example/anthropic".to_string()));
        // No binding (pre-r265) or a deleted profile: DSH's built-in default, as before.
        assert!(endpoint_env(&record(None)).unwrap().is_empty());
        assert!(env("no-such-profile").unwrap().is_empty());
    }

    #[test]
    fn stored_and_env_sources_never_fall_back_to_each_other() {
        let _home = crate::profiles::lifecycle::tests::TestHome::new();
        let created = crate::management::v2::profiles_api::create_from_body(
            br#"{"name":"DSH stored","kind":"dsh","apiKey":"sk-fake-r265-dsh-a"}"#).unwrap();
        let id = created["profile"]["id"].as_str().unwrap().to_string();
        let env_name = "CUTEX_R265_TEST_UNUSED_ENV_NAME_X";
        // Stored: the value comes from the profile key file, under the carrier name.
        let (name, value) = secret_env(&record(Some(binding(&id, "stored", None))), &session(DSH_STORED_KEY_ENV)).unwrap();
        assert_eq!((name.as_str(), value.as_str()), (DSH_STORED_KEY_ENV, "sk-fake-r265-dsh-a"));
        // Rotation: the next start reads the new key.
        let patch = crate::management::v2::profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-dsh-b"}"#).unwrap();
        crate::profiles::settings_api::update_profile(&id, &patch).unwrap();
        let (_, value) = secret_env(&record(Some(binding(&id, "stored", None))), &session(DSH_STORED_KEY_ENV)).unwrap();
        assert_eq!(value, "sk-fake-r265-dsh-b");
        // Redaction (fork/revert pre-projection) resolves the same stored key.
        let mut bound = record(Some(binding(&id, "stored", None)));
        bound.dsh_session = Some(session(DSH_STORED_KEY_ENV));
        assert_eq!(super::super::credential_for_redaction(&bound).as_deref(), Some("sk-fake-r265-dsh-b"));
        // ... and every earlier key of the profile (logs written before the rotation).
        assert_eq!(super::super::credentials_for_redaction(&bound), ["sk-fake-r265-dsh-a", "sk-fake-r265-dsh-b"]);
        // Removed key: refused, no host-variable fallback, no key in the error.
        let patch = crate::management::v2::profiles_api::parse_patch(br#"{"apiKey":null}"#).unwrap();
        crate::profiles::settings_api::update_profile(&id, &patch).unwrap();
        std::env::set_var(DSH_STORED_KEY_ENV, "sk-fake-host-must-not-be-used");
        let error = format!("{:#}", secret_env(&record(Some(binding(&id, "stored", None))), &session(DSH_STORED_KEY_ENV)).unwrap_err());
        std::env::remove_var(DSH_STORED_KEY_ENV);
        assert!(error.contains("credential_missing") && !error.contains("sk-fake"), "{error}");
        // A stored-key launch record without its binding refuses.
        assert!(secret_env(&record(None), &session(DSH_STORED_KEY_ENV)).is_err());
        // Env: reads exactly the named variable, never the stored key.
        let patch = crate::management::v2::profiles_api::parse_patch(br#"{"apiKey":"sk-fake-r265-dsh-c"}"#).unwrap();
        crate::profiles::settings_api::update_profile(&id, &patch).unwrap();
        let error = format!("{:#}", secret_env(&record(Some(binding(&id, "env", Some(env_name)))), &session(env_name)).unwrap_err());
        assert!(error.contains(env_name) && !error.contains("sk-fake"), "{error}");
        // Legacy sessions (no binding) keep the old env behaviour.
        assert!(secret_env(&record(None), &session(env_name)).is_err());
        assert!(secret_env(&record(Some(binding(&id, "env", Some("OTHER")))), &session(env_name)).is_err());
        assert!(secret_env(&record(Some(binding(&id, "future", None))), &session(env_name)).is_err());
    }
}
