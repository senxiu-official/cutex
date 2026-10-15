use std::fs;
use std::path::Path;

use anyhow::anyhow;
use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;

use cutex::config::paths::accounts_path;
use cutex::config::proxy::effective_proxy_config;
use cutex::config::store::load_codez_config;
use cutex::config::text::read_optional_text;
use cutex::config::text::write_optional_text_if_changed;
use cutex::profiles::codex_profile::{inspect_codex_profile_config, CodexProfileConfigSnapshot};
use cutex::profiles::import::detect_source_label;
use cutex::profiles::materialize::custom_status_items_catalog_json;
use cutex::profiles::materialize::ensure_materialized_account_dir;
use cutex::profiles::materialize::legacy_auth_json_from_auth_data;
use cutex::profiles::materialize::materialized_account_files;
use cutex::profiles::materialize::materialized_profiles_dir;
use cutex::profiles::materialize::set_materialized_file_permissions;
use cutex::profiles::model::AccountsStore;
use cutex::profiles::model::AuthData;
use cutex::profiles::model::CliKind;
use cutex::profiles::model::LegacyAccountsStoreV2;
use cutex::profiles::model::ProxyConfig;
use cutex::profiles::model::RuntimeConfig;
use cutex::profiles::model::SessionConfig;
use cutex::profiles::model::StoredAccount;
use cutex::profiles::model::STORE_VERSION;
use cutex::profiles::profile_config::extract_profile_config_toml;
use cutex::profiles::profile_config::merge_and_write_config_toml;
use cutex::profiles::profile_config::normalize_profile_config_for_account;
use cutex::profiles::store::backup_legacy_accounts_file;
use cutex::profiles::store::canonicalize_store;
use cutex::profiles::store::save_store;

use super::profile_usage::ProfileUsageSnapshot;

const RESET: &str = "\x1b[0m";
const YELLOW: &str = "\x1b[33m";

#[derive(Debug, Deserialize)]
struct ReadOnlyProfileCatalog {
    #[serde(default)]
    accounts: Vec<ReadOnlyProfileEntry>,
    active_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadOnlyProfileEntry {
    #[serde(default)]
    options: cutex::profiles::appearance::ProfileOptions,
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    plan_label: Option<String>,
    #[serde(default)]
    quota_preset: cutex::profiles::quota::Preset,
    #[serde(default)]
    quota_reader: Option<cutex::profiles::quota::Reader>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    runtime: RuntimeConfig,
    #[serde(default)]
    proxy: Option<ProxyConfig>,
    #[serde(default)]
    session: Option<SessionConfig>,
    #[serde(default)]
    cli_kind: CliKind,
    #[serde(default)]
    default_cli_args: Vec<String>,
    #[serde(default)]
    agent_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileCatalogEntry {
    pub(crate) options: cutex::profiles::appearance::ProfileOptions,
    pub(crate) quota_usage: Option<String>,
    pub(crate) quota_remaining_percent: Option<u8>,
    pub(crate) quota_balance: Option<String>,
    pub(crate) quota_reset_at: Option<i64>,
    pub(crate) quota_observed_at: Option<i64>,
    pub(crate) quota_stale: bool,
    pub(crate) usage_refreshed_at: Option<i64>,
    pub(crate) usage_error: Option<String>,
    pub(crate) official_usage: Option<ProfileUsageSnapshot>,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) email: Option<String>,
    pub(crate) plan_type: Option<String>,
    pub(crate) plan_label: Option<String>,
    /// Live plan of the profile's own ChatGPT login (Codex): its auth.json
    /// token claim, or a newer native rate-limit observation.
    pub(crate) account_plan: Option<String>,
    pub(crate) quota_preset: cutex::profiles::quota::Choice,
    pub(crate) source: Option<String>,
    pub(crate) runtime: RuntimeConfig,
    pub(crate) proxy: Option<ProxyConfig>,
    pub(crate) session: Option<SessionConfig>,
    pub(crate) cli_kind: String,
    pub(crate) default_cli_args: Vec<String>,
    pub(crate) agent_name: Option<String>,
    pub(crate) api_key_configured: bool,
    pub(crate) codex_config: Option<CodexProfileConfigSnapshot>,
    pub(crate) codex_config_error: Option<String>,
    pub(crate) active: bool,
}

impl ProfileCatalogEntry {
    /// Displayed plan: the manual `plan_label`, else the signed-in ChatGPT
    /// account's live plan, else the legacy `plan_type` (Codex only, as
    /// `plan_label::legacy_plan_type`).
    pub(crate) fn display_plan(&self) -> Option<String> {
        cutex::profiles::plan_label::resolve_plan_label(
            self.account_plan.as_deref(),
            self.plan_label.as_deref(),
            self.plan_type.as_deref().filter(|_| self.cli_kind == "codex"),
        )
        .map(|(label, _)| label)
    }
}

pub(crate) fn load_profile_names_read_only()-> anyhow::Result<Vec<String>> {
    let path = accounts_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read accounts file: {}", path.display()))?;
    Ok(parse_profile_catalog(&data)?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

pub(crate) fn load_profile_catalog_read_only() -> anyhow::Result<Vec<ProfileCatalogEntry>> {
    super::identity_styles::refresh();
    let path = accounts_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read accounts file: {}", path.display()))?;
    let mut entries = parse_profile_catalog(&data)
        .with_context(|| format!("Failed to parse profile catalog: {}", path.display()))?;
    let profiles_dir = materialized_profiles_dir()?;
    for entry in &mut entries {
        if super::profile_usage::refresh_needed(entry) {
            entry.official_usage = Some(ProfileUsageSnapshot::Refreshing);
        }
        if entry.cli_kind != "codex" || entry.id.trim().is_empty() {
            continue;
        }
        let auth_path = profiles_dir.join(&entry.id).join("auth.json");
        entry.account_plan = read_optional_text(&auth_path)
            .ok()
            .flatten()
            .and_then(|raw| cutex::profiles::plan_label::live_plan_for_profile(&entry.id, &raw));
        entry.api_key_configured = entry.source.as_deref() == Some("api-key")
            && auth_file_has_non_empty_api_key(&auth_path);
        let config_path = profiles_dir.join(&entry.id).join("config.toml");
        let config = read_optional_text(&config_path)?;
        let inspected = config
            .as_deref()
            .map(extract_profile_config_toml)
            .transpose()
            .map(Option::flatten)
            .and_then(|config| inspect_codex_profile_config(config.as_deref()));
        match inspected {
            Ok(config) => {
                entry.codex_config = Some(config);
                entry.codex_config_error = None;
            }
            Err(error) => {
                entry.codex_config = None;
                entry.codex_config_error = Some(format!("{}: {error:#}", config_path.display()));
            }
        }
        if entry.options.quota.enabled {
            let quota = entry.options.quota.resolve(
                entry.quota_preset,
                entry
                    .codex_config
                    .as_ref()
                    .and_then(|config| config.provider_base_url.as_deref()),
            );
            let cached = quota.ok().and_then(|quota| {
                let auth = if matches!(
                    quota.auth,
                    cutex::profiles::quota::Auth::ProfileApiKey | cutex::profiles::quota::Auth::ProfileApiKeyRaw
                ) {
                    fs::read(profiles_dir.join(&entry.id).join("auth.json")).unwrap_or_default()
                } else {
                    Vec::new()
                };
                cutex::profiles::quota::cached(&entry.id, &quota, &auth)
            });
            entry.quota_usage = cached.as_ref().map(format_cached_quota_usage);
            if let Some(display) = cached {
                entry.quota_remaining_percent = display
                    .remaining_percent
                    .map(|value| value.round().clamp(0.0, u8::MAX as f64) as u8);
                entry.quota_balance = display.balance.clone();
                entry.quota_reset_at = display.reset_at;
                entry.quota_observed_at = display.observed_at;
                entry.quota_stale = display.stale;
            }
        }
    }
    Ok(entries)
}

fn parse_profile_catalog(data: &str) -> anyhow::Result<Vec<ProfileCatalogEntry>> {
    let catalog: ReadOnlyProfileCatalog = serde_json::from_str(data)?;
    let mut entries = Vec::with_capacity(catalog.accounts.len());
    for entry in catalog.accounts {
        if entry.name.trim().is_empty()
            || entries
                .iter()
                .any(|candidate: &ProfileCatalogEntry| candidate.name == entry.name)
        {
            continue;
        }
        let active = catalog.active_account_id.as_deref() == Some(entry.id.as_str());
        let is_codex = entry.cli_kind == CliKind::Codex;
        let plan_label = entry.plan_label.clone();
        entries.push(ProfileCatalogEntry {
            options: entry.options,
            quota_usage: None,
            quota_remaining_percent: None,
            quota_balance: None,
            quota_reset_at: None,
            quota_observed_at: None,
            quota_stale: false,
            usage_refreshed_at: None,
            usage_error: None,
            official_usage: None,
            id: entry.id,
            name: entry.name,
            email: entry.email,
            plan_type: entry.plan_type,
            plan_label,
            account_plan: None,
            quota_preset: cutex::profiles::quota::Choice::from_parts(entry.quota_preset, entry.quota_reader),
            source: entry.source,
            runtime: entry.runtime,
            proxy: entry.proxy,
            session: entry.session,
            cli_kind: entry.cli_kind.to_string(),
            default_cli_args: entry.default_cli_args,
            agent_name: entry.agent_name,
            api_key_configured: false,
            codex_config: is_codex.then(CodexProfileConfigSnapshot::default),
            codex_config_error: None,
            active,
        });
    }
    Ok(entries)
}

fn format_cached_quota_usage(display: &cutex::profiles::quota::Display) -> String {
    let mut parts = vec![display
        .remaining_percent
        .map(|value| format!("{value:.1}% left"))
        .or_else(|| display.balance.clone())
        .unwrap_or_else(|| "no cached value".to_string())];
    if display.stale {
        parts.push("stale".into());
    }
    if let Some(reset_at) = display.reset_at {
        if let Some(time) = chrono::DateTime::from_timestamp(reset_at, 0) {
            parts.push(format!(
                "reset {}",
                time.with_timezone(&chrono::Local).format("%m-%d %H:%M")
            ));
        }
    }
    if let Some(observed_at) = display.observed_at {
        if let Some(time) = chrono::DateTime::from_timestamp(observed_at, 0) {
            parts.push(format!(
                "seen {}",
                time.with_timezone(&chrono::Local).format("%m-%d %H:%M")
            ));
        }
    }
    parts.join(" · ")
}

fn auth_file_has_non_empty_api_key(path: &Path) -> bool {
    read_optional_text(path)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .is_some_and(|auth| {
            auth.get("OPENAI_API_KEY")
                .or_else(|| auth.get("openai_api_key"))
                .and_then(Value::as_str)
                .is_some_and(|key| !key.trim().is_empty())
        })
}

pub(crate) fn load_store() -> anyhow::Result<AccountsStore> {
    let path = accounts_path()?;
    if !path.exists() {
        return Ok(AccountsStore::default());
    }

    let data = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read accounts file: {}", path.display()))?;
    let json: Value = serde_json::from_str(&data)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))?;
    let version = json
        .get("version")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);

    if version < STORE_VERSION {
        let legacy: LegacyAccountsStoreV2 = serde_json::from_value(json).with_context(|| {
            format!(
                "Failed to parse legacy accounts file for migration: {}",
                path.display()
            )
        })?;
        let migrated = migrate_legacy_store_v2_to_v3(legacy, &path)?;
        save_store(&migrated)?;
        eprintln!(
            "{YELLOW}migrated:{RESET} accounts.json upgraded to v{}; backup saved alongside accounts file",
            STORE_VERSION
        );
        return Ok(migrated);
    }

    let store: AccountsStore = serde_json::from_value(json)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))?;
    Ok(canonicalize_store(
        store,
        detect_source_label_for_account_files,
    ))
}

pub(crate) fn load_store_read_only() -> anyhow::Result<AccountsStore> {
    let path = accounts_path()?;
    if !path.exists() {
        return Ok(AccountsStore::default());
    }

    let data = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read accounts file: {}", path.display()))?;
    parse_store_read_only(&data)
        .with_context(|| format!("Failed to parse accounts file: {}", path.display()))
}

fn parse_store_read_only(data: &str) -> anyhow::Result<AccountsStore> {
    let json: Value = serde_json::from_str(data)?;
    let version = json
        .get("version")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);

    if version < STORE_VERSION {
        anyhow::bail!(
            "accounts.json version {version} requires migration before it can be used for a one-launch profile override; run a profile management command first"
        );
    }

    Ok(serde_json::from_value(json)?)
}

pub(crate) fn detect_source_label_for_account_files(account: &StoredAccount) -> Option<String> {
    let files = materialized_account_files(account).ok()?;
    let auth = read_optional_text(&files.auth_path).ok().flatten();
    let profile_config = read_optional_text(&files.config_path)
        .ok()
        .flatten()
        .and_then(|config| extract_profile_config_toml(&config).ok().flatten());
    Some(detect_source_label(
        auth.as_deref(),
        profile_config.as_deref(),
    ))
}

fn migrate_legacy_store_v2_to_v3(
    legacy: LegacyAccountsStoreV2,
    accounts_file_path: &Path,
) -> anyhow::Result<AccountsStore> {
    backup_legacy_accounts_file(accounts_file_path)?;
    let global_config = load_codez_config();
    let mut migrated = AccountsStore {
        version: STORE_VERSION,
        accounts: Vec::with_capacity(legacy.accounts.len()),
        active_account_id: legacy.active_account_id,
    };

    for legacy_account in legacy.accounts {
        let legacy_profile_config = legacy_account
            .raw_config_toml
            .as_deref()
            .map(extract_profile_config_toml)
            .transpose()?
            .flatten();
        let legacy_source = legacy_account.source.clone().or_else(|| {
            Some(detect_source_label(
                legacy_account.raw_auth_json.as_deref(),
                legacy_profile_config.as_deref(),
            ))
        });

        let account = StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            id: legacy_account.id,
            name: legacy_account.name,
            email: legacy_account.email,
            plan_type: legacy_account.plan_type,
            plan_label: None,
            quota_preset: Default::default(),
            source: legacy_source,
            runtime: legacy_account.runtime,
            proxy: legacy_account.proxy,
            session: legacy_account.session,
            cli_kind: cutex::profiles::model::CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: legacy_account.last_used_at,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
        };

        materialize_migrated_account_files(
            &account,
            legacy_account.raw_auth_json.as_deref(),
            legacy_account.auth.as_ref(),
            legacy_profile_config.as_deref(),
            &global_config,
        )?;
        migrated.accounts.push(account);
    }

    Ok(canonicalize_store(
        migrated,
        detect_source_label_for_account_files,
    ))
}

fn materialize_migrated_account_files(
    account: &StoredAccount,
    legacy_raw_auth_json: Option<&str>,
    legacy_auth_data: Option<&AuthData>,
    legacy_profile_config_toml: Option<&str>,
    global_config: &cutex::profiles::model::CodezConfig,
) -> anyhow::Result<()> {
    let files = materialized_account_files(account)?;
    ensure_materialized_account_dir(&files)?;

    let existing_auth = read_optional_text(&files.auth_path)?;
    let fallback_auth = if let Some(raw_auth_json) = legacy_raw_auth_json {
        Some(raw_auth_json.to_string())
    } else if let Some(auth_data) = legacy_auth_data {
        Some(legacy_auth_json_from_auth_data(auth_data)?)
    } else {
        None
    };
    let auth_contents = existing_auth.or(fallback_auth).ok_or_else(|| {
        anyhow!(
            "Legacy profile '{}' has no auth payload and no materialized auth.json at {}",
            account.name,
            files.auth_path.display()
        )
    })?;
    write_optional_text_if_changed(&files.auth_path, Some(&auth_contents))?;

    let existing_profile_config = read_optional_text(&files.config_path)?.and_then(|existing| {
        match extract_profile_config_toml(&existing) {
            Ok(value) => value,
            Err(err) => {
                eprintln!(
                    "{YELLOW}warning:{RESET} ignoring invalid existing config during migration at {}: {err:#}",
                    files.config_path.display()
                );
                None
            }
        }
    });
    let profile_config =
        existing_profile_config.or_else(|| legacy_profile_config_toml.map(str::to_string));
    let profile_config = normalize_profile_config_for_account(account, profile_config)?;
    merge_and_write_config_toml(
        &files.config_path,
        profile_config.as_deref(),
        effective_proxy_config(account, global_config)
            .map(|proxy| proxy.enabled)
            .unwrap_or(false),
    )?;

    let custom_status_items_json = custom_status_items_catalog_json(global_config)?;
    write_optional_text_if_changed(
        &files.custom_status_items_path,
        custom_status_items_json.as_deref(),
    )?;
    set_materialized_file_permissions(&files)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_profile_catalog, parse_store_read_only};

    #[test]
    fn cached_quota_usage_is_bounded_and_marks_stale_data() {
        let display = cutex::profiles::quota::Display {
            label: "Q".into(),
            remaining_percent: Some(42.56),
            balance: None,
            reset_at: None,
            observed_at: None,
            stale: true,
        };
        assert_eq!(
            super::format_cached_quota_usage(&display),
            "42.6% left · stale"
        );
    }

    #[test]
    fn read_only_catalog_accepts_current_and_legacy_account_shapes() {
        let current = r#"{
            "version": 3,
            "accounts": [
                {"id": "one", "name": "alpha", "cli_kind": "codex"},
                {"id": "two", "name": "beta", "cli_kind": "claude"}
            ]
        }"#;
        let legacy = r#"{
            "version": 2,
            "accounts": [
                {"id": "old", "name": "legacy", "raw_auth_json": "{}"}
            ]
        }"#;

        let current = parse_profile_catalog(current).expect("current catalog");
        assert_eq!(
            current
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(current[0].cli_kind, "codex");
        assert_eq!(current[1].cli_kind, "claude");
        let legacy = parse_profile_catalog(legacy).expect("legacy catalog");
        assert_eq!(legacy[0].name, "legacy");
        assert_eq!(legacy[0].cli_kind, "codex");
    }

    #[test]
    fn read_only_catalog_preserves_order_and_omits_empty_or_duplicate_names() {
        let data = r#"{
            "accounts": [
                {"name": "beta"},
                {"name": " "},
                {"name": "alpha"},
                {"name": "beta"}
            ]
        }"#;

        assert_eq!(
            parse_profile_catalog(data)
                .expect("catalog")
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["beta", "alpha"]
        );
    }

    #[test]
    fn read_only_catalog_projects_metadata_without_deserializing_auth_payloads() {
        let data = r#"{
            "version": 3,
            "active_account_id": "one",
            "accounts": [{
                "id": "one",
                "name": "alpha",
                "email": "alpha@example.test",
                "plan_type": "pro",
                "plan_label": "GLM Pro",
                "source": "official",
                "runtime": {"kind": "docker", "image": "ignored"},
                "proxy": {"enabled": false},
                "session": {"enabled": true},
                "cli_kind": "codex",
                "default_cli_args": ["--model", "gpt-test"],
                "agent_name": "alpha-agent",
                "raw_auth_json": "TOP-SECRET-LEGACY-PAYLOAD"
            }]
        }"#;

        let entries = parse_profile_catalog(data).expect("metadata catalog");
        let entry = &entries[0];
        assert!(entry.active);
        assert_eq!(entry.email.as_deref(), Some("alpha@example.test"));
        assert_eq!(entry.plan_type.as_deref(), Some("pro"));
        assert_eq!(entry.plan_label.as_deref(), Some("GLM Pro"));
        assert_eq!(entry.source.as_deref(), Some("official"));
        assert_eq!(entry.default_cli_args, ["--model", "gpt-test"]);
        assert_eq!(entry.agent_name.as_deref(), Some("alpha-agent"));
    }

    #[test]
    fn read_only_store_accepts_current_store_without_canonicalizing_it() {
        let data = r#"{
            "version": 3,
            "active_account_id": "one",
            "accounts": [{
                "id": "one",
                "name": "alpha",
                "email": null,
                "plan_type": null,
                "runtime": {"kind": "host"},
                "last_used_at": null
            }]
        }"#;

        let store = parse_store_read_only(data).expect("current store");
        assert_eq!(store.active_account_id.as_deref(), Some("one"));
        assert_eq!(store.accounts[0].name, "alpha");
        assert!(store.accounts[0].last_used_at.is_none());
    }

    #[test]
    fn read_only_store_rejects_legacy_store_instead_of_migrating_it() {
        let error = parse_store_read_only(
            r#"{
                "version": 2,
                "active_account_id": null,
                "accounts": []
            }"#,
        )
        .expect_err("legacy store must require an explicit migration");

        assert!(error.to_string().contains("requires migration"));
    }
}
