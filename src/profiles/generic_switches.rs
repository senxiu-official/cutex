//! `profile-generic-switches` (owner decision 2026-10-15): no logic keyed on a
//! specific relay / base-URL domain, provider name, profile name or profile
//! id. Two former special cases became generic per-profile settings the
//! owner sets by hand (`docs/PROFILE-GENERIC-SWITCHES.md`):
//!
//! - `privateChatgptAuthFile` (`StoredAccount::private_chatgpt_auth_file`):
//!   the private native ChatGPT `auth.json` launch mode, formerly bound to
//!   one hard-coded profile id / name, model and effort.
//! - explicit quota readers (`StoredAccount::quota_reader`,
//!   `quota::Reader`), replacing `quota_preset: auto` (resolved by base-URL
//!   host / provider name).
//!
//! The startup migration below is the only place that still knows the old
//! rules. It converges at every `cutex management serve` start:
//!
//! 1. the profile with the formerly hard-coded id AND name whose profile
//!    config still carries the legacy private mode marker gets
//!    `private_chatgpt_auth_file = true` (when not set);
//! 2. every `quota_preset: auto` profile without a reader gets the reader the
//!    old host / provider-name rule resolved to, `quota_preset: custom` and
//!    the resolved endpoint in `options.quota` (so r235 and older keep
//!    reading the same gauge). An `auto` the old rule could not resolve is
//!    left as it is (it showed `presetError` before; now it reports the
//!    removed preset) and listed in the record.
//!
//! Re-running is a no-op (nothing matches any more). A run that changes the
//! accounts file first copies it to
//! `archive/profile-generic-switches/accounts.pre-profile-generic-switches-<UTC>.json`
//! and appends the run to `archive/profile-generic-switches/migration.json`
//! (also written once when nothing needed a change). Errors never fail
//! startup; the next start retries.

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use crate::profiles::model::{AccountsStore, CliKind, StoredAccount};
use crate::profiles::quota::{Choice, Preset, Reader};

pub const RECORD_SCHEMA: &str = "cutex/profile-generic-switches-migration/v1";
const ARCHIVE_ROOT: &str = "archive/profile-generic-switches";
const RECORD_FILE: &str = "migration.json";

/// Identifiers written by releases before `profile-generic-switches`. They
/// stay byte-for-byte because persisted data (launch receipts, profile
/// configs) carries them; no code path decides anything by them except the
/// migration in this module.
pub mod legacy {
    /// Domain separator of the reviewed ChatGPT account digest
    /// (`launch::chatgpt_auth_file::account_identity`). The digest is part of
    /// persisted launch reviews / selected-profile projections, so changing
    /// it would invalidate every reviewed ChatGPT runtime.
    pub const ACCOUNT_DIGEST_DOMAIN: &str = "cutex-chatgpt-account-user-v1";
    /// Profile config (`config.toml`) `cutex_provider_mode` value of the
    /// private ChatGPT mode written before profile-generic-switches. Still accepted next to the
    /// switch; without the switch it is refused with a pointer to it.
    pub const PRIVATE_CHATGPT_MODE: &str = "aemeath_chatgpt_v1";
    /// The one profile the old code accepted for that mode (id AND name).
    /// The name is matched by its SHA-256 (hex), like the relay domain below:
    /// it is the owner's profile name and not shipped in source.
    pub(super) const PRIVATE_CHATGPT_PROFILE_ID: &str = "cd6a39eb-3997-45c6-9824-5113fe36a4b8";
    pub(super) const PRIVATE_CHATGPT_PROFILE_NAME_SHA256: &str =
        "914688da7fe3a1357e44f3c6b08e21e908afec03bffff7af3eb9b1498f2619ec";
    /// SHA-256 (hex) of the registrable domain of the relay whose
    /// `<base URL>/usage` reader `quota_preset: auto` picked by host. The
    /// domain itself is not shipped in source.
    pub(super) const USAGE_RELAY_DOMAIN_SHA256: &str =
        "03d2f3217c132b216daf19200ebe86a1734fbad3294ac69ca3506981109cba3f";
}

/// One profile the migration changed or could not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    PrivateChatgptAuthFile { profile: String },
    QuotaReader { profile: String, reader: Reader },
    UnresolvedAuto { profile: String, reason: &'static str },
}

impl Change {
    fn changes_store(&self) -> bool {
        !matches!(self, Change::UnresolvedAuto { .. })
    }

    fn json(&self) -> Value {
        match self {
            Change::PrivateChatgptAuthFile { profile } => {
                json!({"profile": profile, "set": "private_chatgpt_auth_file"})
            }
            Change::QuotaReader { profile, reader } => {
                json!({"profile": profile, "set": "quota_reader", "reader": reader.as_str()})
            }
            Change::UnresolvedAuto { profile, reason } => {
                json!({"profile": profile, "unresolvedAuto": reason})
            }
        }
    }
}

/// `cutex management serve` calls this. Never fails or panics.
pub fn run_on_startup(component: &str) {
    let result = std::panic::catch_unwind(|| {
        run_at(Utc::now(), legacy::USAGE_RELAY_DOMAIN_SHA256, legacy::PRIVATE_CHATGPT_PROFILE_NAME_SHA256)
    });
    match result {
        Ok(Ok(changes)) => {
            for change in &changes {
                eprintln!("cutex profile-generic-switches: {}", change.json());
            }
        }
        Ok(Err(error)) => eprintln!(
            "cutex profile-generic-switches migration: {component}: not run on this start: {error:#}"
        ),
        Err(_) => eprintln!(
            "cutex profile-generic-switches migration: {component}: panicked, not run on this start"
        ),
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
}

/// Whether `host` is `domain` or a subdomain of it, with `domain` given as
/// its SHA-256 (hex).
pub(crate) fn host_matches_sha256(host: &str, domain_sha256: &str) -> bool {
    use sha2::Digest;
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let mut candidate = host.as_str();
    loop {
        if format!("{:x}", sha2::Sha256::digest(candidate.as_bytes())) == domain_sha256 {
            return true;
        }
        match candidate.split_once('.') {
            Some((_, rest)) if rest.contains('.') => candidate = rest,
            _ => return false,
        }
    }
}

/// The reader `quota_preset: auto` resolved to before profile-generic-switches (host first, then
/// the provider-name rules of profiles without a base URL), or why none.
fn legacy_auto_reader(
    provider: Option<&str>,
    base_url: Option<&str>,
    relay_sha256: &str,
) -> Result<Reader, &'static str> {
    let provider = provider.unwrap_or_default().to_ascii_lowercase();
    let base_url = base_url.map(str::trim).filter(|value| !value.is_empty());
    let Some(base_url) = base_url else {
        if ["kimi", "glm", "zai"].iter().any(|name| provider.contains(name)) {
            // Earlier releases filled in the relay's default base URL; a
            // generic reader has no default URL to read.
            return Err("no base URL (the old rule used a relay default URL); set the quota URL by hand");
        }
        if provider.contains("deepseek") {
            return Ok(Reader::DeepseekBalance);
        }
        return Err("no automatic reader matched before either");
    };
    let host = url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.trim_end_matches('.').to_ascii_lowercase()));
    let Some(host) = host else {
        return Err("no automatic reader matched before either");
    };
    use crate::profiles::providers::host_matches;
    if host_matches_sha256(&host, relay_sha256) {
        return Ok(Reader::UsageEndpoint);
    }
    if host_matches(&host, "deepseek.com") {
        return Ok(Reader::DeepseekBalance);
    }
    match host.as_str() {
        "api.moonshot.ai" => Ok(Reader::MoonshotBalance),
        "api.moonshot.cn" => Ok(Reader::MoonshotCnBalance),
        "api.z.ai" | "open.bigmodel.cn" | "bigmodel.cn" => Ok(Reader::ZhipuCodingPlan),
        "openrouter.ai" => Ok(Reader::OpenrouterKey),
        _ => Err("no automatic reader matched before either"),
    }
}

fn profile_mode_marker(account: &StoredAccount) -> Option<String> {
    let files = crate::profiles::materialize::materialized_account_files(account).ok()?;
    let raw = std::fs::read_to_string(files.config_path).ok()?;
    let table: toml::Table = toml::from_str(&raw).ok()?;
    table.get("cutex_provider_mode")?.as_str().map(str::to_owned)
}

/// Plans the migration of one typed account; `value` is its JSON object in
/// the accounts file (changed in place).
fn plan_account(
    account: &StoredAccount,
    value: &mut Value,
    relay_sha256: &str,
    private_name_sha256: &str,
) -> anyhow::Result<Vec<Change>> {
    let mut changes = Vec::new();
    let profile = account.name.clone();
    if account.cli_kind == CliKind::Codex
        && !account.private_chatgpt_auth_file
        && account.id == legacy::PRIVATE_CHATGPT_PROFILE_ID
        && sha256_hex(&account.name) == private_name_sha256
        && profile_mode_marker(account).as_deref() == Some(legacy::PRIVATE_CHATGPT_MODE)
    {
        value["private_chatgpt_auth_file"] = json!(true);
        changes.push(Change::PrivateChatgptAuthFile { profile: profile.clone() });
    }
    if account.quota_preset == Preset::Auto && account.quota_reader.is_none() {
        let provider = crate::profiles::inspect::account_model_provider(account);
        let base_url = crate::profiles::inspect::account_model_api_base(account);
        match legacy_auto_reader(provider.as_deref(), base_url.as_deref(), relay_sha256) {
            Ok(reader) => {
                let mut primary = account.options.quota.clone();
                // The old `/usage` reader labelled its gauge by provider name.
                if reader == Reader::UsageEndpoint && primary.label == "Q" {
                    let lower = provider.as_deref().unwrap_or_default().to_ascii_lowercase();
                    primary.label = if lower.contains("kimi") || lower.contains("moonshot") { "Kimi" } else { "GLM" }.into();
                }
                match primary.resolve(Choice::Reader(reader), base_url.as_deref()) {
                    Ok(resolved) => {
                        value["quota_preset"] = json!("custom");
                        value["quota_reader"] = json!(reader.as_str());
                        value["options"]["quota"] = serde_json::to_value(&resolved)?;
                        changes.push(Change::QuotaReader { profile, reader });
                    }
                    Err(_) => changes.push(Change::UnresolvedAuto {
                        profile,
                        reason: "the matched reader does not resolve for this profile",
                    }),
                }
            }
            Err(reason) => changes.push(Change::UnresolvedAuto { profile, reason }),
        }
    }
    Ok(changes)
}

/// Runs the migration on the current cutex home. Returns every change and
/// unresolved `auto` profile.
pub fn run_at(now: DateTime<Utc>, relay_sha256: &str, private_name_sha256: &str) -> anyhow::Result<Vec<Change>> {
    let cutex_dir = crate::config::paths::config_dir()?;
    let accounts = crate::config::paths::accounts_path()?;
    let archive = cutex_dir.join(ARCHIVE_ROOT);
    let record_path = archive.join(RECORD_FILE);
    let mut changes = Vec::new();
    let mut backup = Value::Null;
    if accounts.exists() {
        let raw = std::fs::read(&accounts).with_context(|| format!("read {}", accounts.display()))?;
        let mut store: Value =
            serde_json::from_slice(&raw).with_context(|| format!("parse {}", accounts.display()))?;
        let typed: AccountsStore =
            serde_json::from_slice(&raw).with_context(|| format!("parse {}", accounts.display()))?;
        let values = store
            .get_mut("accounts")
            .and_then(Value::as_array_mut)
            .context("accounts file has no accounts array")?;
        anyhow::ensure!(values.len() == typed.accounts.len(), "accounts file changed during the migration");
        for (account, value) in typed.accounts.iter().zip(values.iter_mut()) {
            changes.extend(plan_account(account, value, relay_sha256, private_name_sha256)?);
        }
        if changes.iter().any(Change::changes_store) {
            crate::management::client_tokens::ensure_private_dir(&cutex_dir.join("archive"))?;
            crate::management::client_tokens::ensure_private_dir(&archive)?;
            let copy = archive.join(format!(
                "accounts.pre-profile-generic-switches-{}.json",
                now.format("%Y%m%dT%H%M%SZ")
            ));
            crate::config::atomic::write_private_bytes_atomic(&copy, &raw)?;
            backup = json!(copy.to_string_lossy());
            crate::config::atomic::write_pretty_json_atomic(&accounts, &store, "accounts file")?;
        }
    }
    if changes.iter().any(Change::changes_store) || !record_path.exists() {
        crate::management::client_tokens::ensure_private_dir(&cutex_dir.join("archive"))?;
        crate::management::client_tokens::ensure_private_dir(&archive)?;
        let mut record = std::fs::read(&record_path)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .filter(|record| record["runs"].is_array())
            .unwrap_or_else(|| json!({"schema": RECORD_SCHEMA, "runs": []}));
        record["runs"].as_array_mut().expect("runs array").push(json!({
            "at": now.to_rfc3339_opts(SecondsFormat::Secs, true),
            "backup": backup,
            "profiles": changes.iter().map(Change::json).collect::<Vec<_>>(),
        }));
        crate::config::atomic::write_private_pretty_json_atomic(
            &record_path,
            &record,
            "profile-generic-switches record",
        )?;
    }
    Ok(changes)
}

/// The account digest the stock launch review binds: the serialized account
/// without the private ChatGPT switch (its effect is the reviewed auth in the
/// launch configuration). An account without the switch digests exactly as
/// before, so turning on the switch by migration does not invalidate an
/// existing reviewed launch.
pub fn launch_account_digest_input(account: &StoredAccount) -> anyhow::Result<Vec<u8>> {
    if !account.private_chatgpt_auth_file {
        return Ok(serde_json::to_vec(account)?);
    }
    let mut copy = account.clone();
    copy.private_chatgpt_auth_file = false;
    Ok(serde_json::to_vec(&copy)?)
}

/// Validation the switch needs on top of the builtin-provider rule: the
/// launch uses the profile's own configured model and effort (no hard-coded
/// pin, no native fallback). A selection may only repeat them.
pub fn validate_private_chatgpt_model(
    profile_model: &str,
    profile_effort: Option<&str>,
    selected_model: Option<&str>,
    selected_effort: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !profile_model.trim().is_empty(),
        "privateChatgptAuthFile requires the profile's model to be set"
    );
    anyhow::ensure!(
        selected_model.is_none_or(|model| model == profile_model),
        "privateChatgptAuthFile launches the profile's own model ({profile_model}); no other model"
    );
    let effort = profile_effort.or(selected_effort);
    anyhow::ensure!(
        effort.is_some_and(|effort| !effort.trim().is_empty()),
        "privateChatgptAuthFile requires the profile's model_reasoning_effort to be set"
    );
    anyhow::ensure!(
        profile_effort.is_none() || selected_effort.is_none_or(|effort| Some(effort) == profile_effort),
        "privateChatgptAuthFile launches the profile's own reasoning effort; no other effort"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::lifecycle::tests::TestHome;
    use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
    use crate::profiles::providers::find_preset;

    fn sha(text: &str) -> String {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
    }

    #[test]
    fn hashed_domain_matches_the_domain_and_subdomains_only() {
        let relay = sha("relay.example");
        for host in ["relay.example", "www.relay.example", "A.B.Relay.Example.", "api.relay.example"] {
            assert!(host_matches_sha256(host, &relay), "{host}");
        }
        for host in ["evilrelay.example", "relay.example.org", "example", "relay-example.com"] {
            assert!(!host_matches_sha256(host, &relay), "{host}");
        }
    }

    #[test]
    fn legacy_auto_rules_map_to_explicit_readers() {
        let relay = sha("relay.example");
        let rule = |provider: &str, base: Option<&str>| legacy_auto_reader(Some(provider), base, &relay);
        assert_eq!(rule("custom", Some("https://www.relay.example/v1")), Ok(Reader::UsageEndpoint));
        assert_eq!(rule("x", Some("https://api.deepseek.com/v1")), Ok(Reader::DeepseekBalance));
        assert_eq!(rule("kimi", Some("https://api.moonshot.ai/v1")), Ok(Reader::MoonshotBalance));
        assert_eq!(rule("kimi", Some("https://api.moonshot.cn/v1")), Ok(Reader::MoonshotCnBalance));
        assert_eq!(rule("zai", Some("https://open.bigmodel.cn/api/v1")), Ok(Reader::ZhipuCodingPlan));
        assert_eq!(rule("x", Some("https://openrouter.ai/api/v1")), Ok(Reader::OpenrouterKey));
        assert_eq!(rule("deepseek", None), Ok(Reader::DeepseekBalance));
        assert!(rule("kimi", None).is_err(), "relay default URL is gone");
        assert!(rule("kimi", Some("https://proxy.example/v1")).is_err());
        assert!(rule("x", Some("https://notrelay.example/v1")).is_err());
    }

    #[test]
    fn private_chatgpt_model_uses_the_profile_values() {
        assert!(validate_private_chatgpt_model("m", Some("low"), None, None).is_ok());
        assert!(validate_private_chatgpt_model("m", Some("low"), Some("m"), Some("low")).is_ok());
        assert!(validate_private_chatgpt_model("m", None, None, Some("high")).is_ok());
        assert!(validate_private_chatgpt_model("m", Some("low"), Some("other"), None).is_err());
        assert!(validate_private_chatgpt_model("m", Some("low"), None, Some("high")).is_err());
        assert!(validate_private_chatgpt_model("m", None, None, None).is_err());
        assert!(validate_private_chatgpt_model(" ", Some("low"), None, None).is_err());
    }

    fn write_profile_config(account: &StoredAccount, extra: &str) {
        let files = crate::profiles::materialize::materialized_account_files(account).unwrap();
        std::fs::create_dir_all(files.config_path.parent().unwrap()).unwrap();
        std::fs::write(&files.config_path, format!("model='m'\nmodel_provider='openai'\n{extra}")).unwrap();
    }

    #[test]
    fn migration_converts_legacy_state_once_and_records_it() {
        let _home = TestHome::new();
        let relay = sha("relay.example");
        let api = |name: &str, preset: &str, base: Option<&str>| NewProfile {
            name: name.into(),
            kind: NewProfileKind::ApiKey,
            preset: find_preset(preset),
            base_url: base.map(str::to_owned),
            model: None,
            api_key: None,
        };
        let relayed = create_profile(&api("alpha", "custom", Some("https://www.relay.example/v1"))).unwrap();
        let kimi_relayed = create_profile(&api("beta", "custom", Some("https://relay.example/v1/"))).unwrap();
        let deepseek = create_profile(&api("gamma", "custom", Some("https://api.deepseek.com/v1"))).unwrap();
        let unknown = create_profile(&api("delta", "custom", Some("https://proxy.example/v1"))).unwrap();
        let chatgpt = create_profile(&NewProfile {
            name: "epsilon".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: None,
            api_key: None,
        })
        .unwrap();
        // Legacy state as an older release stored it.
        let accounts = crate::config::paths::accounts_path().unwrap();
        let mut store: Value = serde_json::from_slice(&std::fs::read(&accounts).unwrap()).unwrap();
        for value in store["accounts"].as_array_mut().unwrap() {
            if value["name"] != "epsilon" {
                value["quota_preset"] = json!("auto");
                value["options"]["quota"]["enabled"] = json!(true);
            }
        }
        std::fs::write(&accounts, serde_json::to_vec(&store).unwrap()).unwrap();
        // The private ChatGPT marker on a profile with another id: untouched.
        write_profile_config(&chatgpt, &format!("cutex_provider_mode='{}'\n", legacy::PRIVATE_CHATGPT_MODE));

        let now = DateTime::parse_from_rfc3339("2026-10-15T12:00:00Z").unwrap().with_timezone(&Utc);
        let changes = run_at(now, &relay, &sha("no-legacy-profile")).unwrap();
        assert!(changes.contains(&Change::QuotaReader { profile: "alpha".into(), reader: Reader::UsageEndpoint }));
        assert!(changes.contains(&Change::QuotaReader { profile: "beta".into(), reader: Reader::UsageEndpoint }));
        assert!(changes.contains(&Change::QuotaReader { profile: "gamma".into(), reader: Reader::DeepseekBalance }));
        assert!(changes.iter().any(|c| matches!(c, Change::UnresolvedAuto { profile, .. } if profile == "delta")));
        assert!(!changes.iter().any(|c| matches!(c, Change::PrivateChatgptAuthFile { .. })));

        let after: AccountsStore = serde_json::from_slice(&std::fs::read(&accounts).unwrap()).unwrap();
        let find = |id: &str| after.accounts.iter().find(|a| a.id == id).unwrap().clone();
        let alpha = find(&relayed.id);
        assert_eq!((alpha.quota_preset, alpha.quota_reader), (Preset::Custom, Some(Reader::UsageEndpoint)));
        assert_eq!(alpha.options.quota.url, "https://www.relay.example/v1/usage", "older binaries read it");
        assert_eq!(alpha.options.quota.label, "GLM", "the old provider-name label is kept");
        assert!(alpha.options.quota.enabled);
        assert_eq!(crate::profiles::quota::config_view(&alpha)["resolvedPrimary"]["url"], "https://www.relay.example/v1/usage");
        let gamma = find(&deepseek.id);
        assert_eq!(gamma.options.quota.url, "https://api.deepseek.com/user/balance");
        let delta = find(&unknown.id);
        assert_eq!((delta.quota_preset, delta.quota_reader), (Preset::Auto, None), "left for the owner");
        assert!(!find(&chatgpt.id).private_chatgpt_auth_file);
        let _ = kimi_relayed;

        let archive = crate::config::paths::config_dir().unwrap().join(ARCHIVE_ROOT);
        assert!(archive.join("accounts.pre-profile-generic-switches-20261015T120000Z.json").exists());
        let record: Value = serde_json::from_slice(&std::fs::read(archive.join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!(record["schema"], RECORD_SCHEMA);
        assert_eq!(record["runs"].as_array().unwrap().len(), 1);

        // Idempotent: the second start changes nothing and writes no run.
        let before = std::fs::read(&accounts).unwrap();
        let again = run_at(now, &relay, &sha("no-legacy-profile")).unwrap();
        assert!(again.iter().all(|change| !change.changes_store()), "{again:?}");
        assert_eq!(std::fs::read(&accounts).unwrap(), before);
        let record: Value = serde_json::from_slice(&std::fs::read(archive.join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!(record["runs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn migration_sets_the_private_switch_on_the_legacy_profile_only() {
        let _home = TestHome::new();
        let created = create_profile(&NewProfile {
            name: "zeta".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: None,
            api_key: None,
        })
        .unwrap();
        // Rewrite the stored id to the legacy one (fixture: the migration's
        // own constant); the legacy name is matched by hash, so the test
        // passes the hash of this profile's name.
        let accounts = crate::config::paths::accounts_path().unwrap();
        let mut store: Value = serde_json::from_slice(&std::fs::read(&accounts).unwrap()).unwrap();
        let value = store["accounts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|value| value["id"] == created.id.as_str())
            .unwrap();
        value["id"] = json!(legacy::PRIVATE_CHATGPT_PROFILE_ID);
        std::fs::write(&accounts, serde_json::to_vec(&store).unwrap()).unwrap();
        let typed: AccountsStore = serde_json::from_slice(&std::fs::read(&accounts).unwrap()).unwrap();
        let legacy_account = typed.accounts.iter().find(|a| a.id == legacy::PRIVATE_CHATGPT_PROFILE_ID).unwrap().clone();
        let now = Utc::now();
        // Without the marker in its config: nothing.
        write_profile_config(&legacy_account, "");
        assert!(run_at(now, &sha("relay.example"), &sha("zeta")).unwrap().is_empty());
        write_profile_config(&legacy_account, &format!("cutex_provider_mode='{}'\n", legacy::PRIVATE_CHATGPT_MODE));
        let digest_before = launch_account_digest_input(&legacy_account).unwrap();
        let changes = run_at(now, &sha("relay.example"), &sha("zeta")).unwrap();
        assert_eq!(
            changes,
            vec![Change::PrivateChatgptAuthFile { profile: "zeta".into() }]
        );
        let after: AccountsStore = serde_json::from_slice(&std::fs::read(&accounts).unwrap()).unwrap();
        let migrated = after.accounts.iter().find(|a| a.id == legacy::PRIVATE_CHATGPT_PROFILE_ID).unwrap();
        assert!(migrated.private_chatgpt_auth_file);
        assert_eq!(
            launch_account_digest_input(migrated).unwrap(),
            digest_before,
            "the launch account digest does not move"
        );
        assert!(run_at(now, &sha("relay.example"), &sha("zeta")).unwrap().is_empty(), "idempotent");
    }
}
