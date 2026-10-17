//! Copy a profile to another host (`profile-transfer`): export a profile's
//! settings as a bundle (schemaVersion 1) on host A and import that bundle as
//! a NEW profile on host B. The desktop moves the bundle main process to main
//! process (A → B); cutex hosts never talk to each other for this.
//!
//! What moves (cutex-desktop docs/research/profile-migration-design-20261016.md):
//!
//! - the store record, minus identity / derived / host-bound fields (id,
//!   email, last use, the ChatGPT plan, the Claude account id);
//! - the profile part of `config.toml` and the effective model catalog plus
//!   overlay (as `lifecycle::duplicate_profile` does on one host);
//! - only when asked AND only over a loopback connection (checked by the
//!   route): an API-key profile's API key, and secrets that may sit in the
//!   settings (proxy user/password, provider bearer token / HTTP headers,
//!   `shell_environment_policy.set`). Without credentials those are dropped
//!   and the bundle lists what was dropped.
//!
//! Never: a ChatGPT login (refresh tokens rotate, two hosts would log each
//! other out: log in again on the target), Claude account credentials or
//! Claude API account keys (the copy binds the target's account with the same
//! label and kind, else the target's default account), quota caches, usage
//! or session references. Import never overwrites: a name clash is a 409.
//! Unknown bundle fields are ignored (additive schema).

use serde_json::{json, Map, Value};

use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::text::read_optional_text;
use crate::profiles::catalog_overlay::{self, Overlay, OVERLAY_VERSION};
use crate::profiles::lifecycle::{self, LifecycleError};
use crate::profiles::materialize::{
    ensure_materialized_account_dir, materialize_new_account_files, materialized_account_files,
};
use crate::profiles::model::{CliKind, RuntimeConfig, StoredAccount};
use crate::profiles::profile_config::{
    extract_profile_config_toml, parse_toml_table, profile_uses_local_model_catalog,
};
use crate::profiles::settings_api::{find, load_store, ApiKey, MAX_API_KEY_BYTES};
use crate::profiles::store::save_store;
use crate::runtime::claude_accounts::{self, ClaudeAccountKind};

pub const BUNDLE_FORMAT: &str = "cutex-profile-bundle";
pub const BUNDLE_SCHEMA_VERSION: u64 = 1;
/// Audit log of exports / imports (no values), under the runtime dir.
pub const AUDIT_DIR: &str = "profile-transfer";

/// Store-record keys never exported (identity, derived state, host-local ids).
const IDENTITY_KEYS: &[&str] = &["id", "name", "email", "last_used_at", "claude_account"];

/// The credential a profile uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    /// Codex profile on an API key (`auth.json` `OPENAI_API_KEY`).
    ApiKey,
    /// Codex profile on a ChatGPT login (OAuth tokens) or the host's private
    /// native `auth.json`.
    Chatgpt,
    /// Claude profile: the credential belongs to a Claude account.
    Claude,
    /// Nothing recognised (e.g. a custom profile without auth).
    None,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "apiKey",
            Self::Chatgpt => "chatgpt",
            Self::Claude => "claude",
            Self::None => "none",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "apiKey" => Some(Self::ApiKey),
            "chatgpt" => Some(Self::Chatgpt),
            "claude" => Some(Self::Claude),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum TransferError {
    NotFound,
    /// 400 `invalid_request`.
    Invalid(String),
    /// 400 `unsupported_bundle`: the bundle is not one this cutex reads.
    Unsupported(String),
    /// 409 `profile_name_conflict`.
    NameConflict(String),
    Store(anyhow::Error),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("no profile has this ID or name"),
            Self::Invalid(message) | Self::Unsupported(message) => f.write_str(message),
            Self::NameConflict(name) => write!(f, "a profile named {name} already exists"),
            Self::Store(error) => write!(f, "{error:#}"),
        }
    }
}

impl From<anyhow::Error> for TransferError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

impl From<LifecycleError> for TransferError {
    fn from(error: LifecycleError) -> Self {
        match error {
            LifecycleError::NotFound => Self::NotFound,
            LifecycleError::NameConflict(name) => Self::NameConflict(name),
            LifecycleError::Store(error) => Self::Store(error),
            other => Self::Invalid(other.to_string()),
        }
    }
}

fn unsupported(message: impl Into<String>) -> TransferError {
    TransferError::Unsupported(message.into())
}

/// The parsed `auth.json` of a Codex profile (never logged).
fn read_auth(account: &StoredAccount) -> anyhow::Result<Option<Value>> {
    let files = materialized_account_files(account)?;
    Ok(read_optional_text(&files.auth_path)?.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()))
}

fn api_key_of(auth: &Value) -> Option<&str> {
    let has_tokens = auth.get("tokens").is_some_and(|tokens| !tokens.is_null());
    (!has_tokens)
        .then(|| auth.get("OPENAI_API_KEY").and_then(Value::as_str))
        .flatten()
        .filter(|key| !key.trim().is_empty())
}

/// Which credential `account` uses (from its auth.json, else its source).
pub fn credential_kind(account: &StoredAccount) -> CredentialKind {
    if account.cli_kind == CliKind::Claude {
        return CredentialKind::Claude;
    }
    if account.private_chatgpt_auth_file {
        return CredentialKind::Chatgpt;
    }
    if let Ok(Some(auth)) = read_auth(account) {
        if auth.get("tokens").is_some_and(Value::is_object) {
            return CredentialKind::Chatgpt;
        }
        if api_key_of(&auth).is_some() {
            return CredentialKind::ApiKey;
        }
    }
    match account.source.as_deref() {
        Some("official") => CredentialKind::Chatgpt,
        Some("api-key") => CredentialKind::ApiKey,
        _ => CredentialKind::None,
    }
}

fn url_has_userinfo(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => !parsed.username().is_empty() || parsed.password().is_some(),
        // Unparseable: treat anything that looks like userinfo as a secret.
        Err(_) => url.contains('@'),
    }
}

/// Secrets that may sit in the profile part of `config.toml`. Removed unless
/// credentials travel; returns the dropped field paths.
fn strip_config_secrets(root: &mut toml::value::Table, dropped: &mut Vec<String>) {
    if let Some(providers) = root.get_mut("model_providers").and_then(toml::Value::as_table_mut) {
        for (id, provider) in providers.iter_mut() {
            let Some(provider) = provider.as_table_mut() else { continue };
            for key in ["experimental_bearer_token", "http_headers"] {
                if provider.remove(key).is_some() {
                    dropped.push(format!("model_providers.{id}.{key}"));
                }
            }
        }
    }
    if let Some(policy) = root.get_mut("shell_environment_policy").and_then(toml::Value::as_table_mut) {
        if policy.remove("set").is_some() {
            dropped.push("shell_environment_policy.set".into());
        }
    }
}

fn export_codex(account: &StoredAccount, with_secrets: bool, dropped: &mut Vec<String>) -> anyhow::Result<Value> {
    let files = materialized_account_files(account)?;
    let profile_toml = match read_optional_text(&files.config_path)? {
        Some(raw) => extract_profile_config_toml(&raw)?,
        None => None,
    };
    let Some(text) = profile_toml else {
        return Ok(json!({ "config": null, "catalog": null }));
    };
    let mut root = parse_toml_table(&text)?;
    let mut catalog = Value::Null;
    if root.contains_key("model_catalog_json") {
        if profile_uses_local_model_catalog(&text)? {
            // The profile-local models.json (a missing one of a preset
            // provider is the preset's own: the target writes it itself).
            if let Some(local) = read_optional_text(&files.model_catalog_path)? {
                let json: Value = serde_json::from_str(&local)
                    .map_err(|_| anyhow::anyhow!("the profile's models.json is not JSON"))?;
                catalog = json!({ "kind": "local", "json": json });
            }
        } else {
            // A selected immutable catalog file (absolute host path): send the
            // effective catalog and the overlay; the target writes its own file.
            root.remove("model_catalog_json");
            if let Some(json) = crate::profiles::catalog_api::load(account)?.catalog {
                let overlay = catalog_overlay::load(account)?;
                catalog = json!({ "kind": "selected", "json": json, "overlay": overlay });
            }
        }
    }
    if !with_secrets {
        strip_config_secrets(&mut root, dropped);
    }
    Ok(json!({ "config": toml::to_string_pretty(&root)?, "catalog": catalog }))
}

#[derive(Debug)]
pub struct Export {
    /// The bundle; holds the API key when `credentials_included`. Never log it.
    pub bundle: Value,
    pub credentials_included: bool,
    pub credential_kind: CredentialKind,
    pub profile_id: String,
    pub profile_name: String,
}

/// Export `profile` (ID or name). With `include_credentials` (the caller has
/// checked the connection is loopback) an API-key profile's key and settings
/// secrets are included; a ChatGPT login never is.
pub fn export_profile(profile: &str, include_credentials: bool) -> Result<Export, TransferError> {
    let store = load_store()?;
    let account = find(&store, profile).cloned().ok_or(TransferError::NotFound)?;
    let kind = credential_kind(&account);
    let mut dropped: Vec<String> = Vec::new();

    let mut record = serde_json::to_value(&account).map_err(anyhow::Error::from)?;
    let object = record.as_object_mut().ok_or_else(|| anyhow::anyhow!("profile record is not an object"))?;
    for key in IDENTITY_KEYS {
        object.remove(*key);
    }
    if account.source.as_deref() != Some("api-key") {
        // ChatGPT plan / its label belong to the login, not the settings.
        object.remove("plan_type");
        object.remove("plan_label");
    }
    if !include_credentials
        && account.proxy.as_ref().and_then(|proxy| proxy.url.as_deref()).is_some_and(url_has_userinfo)
    {
        object.remove("proxy");
        dropped.push("proxy".into());
    }

    let codex = match account.cli_kind {
        CliKind::Codex => export_codex(&account, include_credentials, &mut dropped)?,
        CliKind::Claude => Value::Null,
    };
    let claude_account = match account.claude_account.as_deref() {
        None | Some(claude_accounts::DEFAULT_ACCOUNT_ID) => Value::Null,
        Some(id) => claude_accounts::load()
            .ok()
            .and_then(|registry| registry.find(id).map(|found| json!({ "label": found.label, "kind": found.kind.as_str() })))
            .unwrap_or(Value::Null),
    };

    let mut credentials_included = false;
    let mut credentials = Value::Null;
    if include_credentials && kind == CredentialKind::ApiKey {
        if let Some(auth) = read_auth(&account)? {
            if let Some(key) = api_key_of(&auth) {
                credentials = json!({ "kind": "apiKey", "apiKey": key });
                credentials_included = true;
            }
        }
    }

    let bundle = json!({
        "format": BUNDLE_FORMAT,
        "schemaVersion": BUNDLE_SCHEMA_VERSION,
        "exportedAt": chrono::Utc::now().to_rfc3339(),
        "source": {
            "hostId": crate::platform::host::current_host_name(),
            "profileId": account.id,
            "profileName": account.name,
        },
        "credentialKind": kind.as_str(),
        "profile": record,
        "claudeAccount": claude_account,
        "codex": codex,
        "dropped": dropped,
        "credentials": credentials,
    });
    Ok(Export {
        bundle,
        credentials_included,
        credential_kind: kind,
        profile_id: account.id,
        profile_name: account.name,
    })
}

enum CatalogPart {
    Local(Value),
    Selected { json: Value, overlay: Option<Overlay> },
}

struct ParsedBundle {
    account: StoredAccount,
    kind: CredentialKind,
    api_key: Option<ApiKey>,
    claude: Option<(String, ClaudeAccountKind)>,
    config: Option<String>,
    catalog: Option<CatalogPart>,
    dropped: Vec<String>,
    source_name: Option<String>,
}

fn catalog_json(value: &Value) -> Result<Value, TransferError> {
    if value.get("models").is_some_and(Value::is_array) {
        Ok(value.clone())
    } else {
        Err(unsupported("codex.catalog.json: a model catalog object with a models array is required"))
    }
}

fn parse_catalog(value: Option<&Value>) -> Result<Option<CatalogPart>, TransferError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let json = catalog_json(value.get("json").unwrap_or(&Value::Null))?;
    match value.get("kind").and_then(Value::as_str) {
        Some("local") => Ok(Some(CatalogPart::Local(json))),
        Some("selected") => {
            let overlay = match value.get("overlay") {
                None | Some(Value::Null) => None,
                Some(overlay) => {
                    // No serde text in the message (it may quote values).
                    let overlay: Overlay = serde_json::from_value(overlay.clone()).map_err(|_| {
                        unsupported("codex.catalog.overlay: a model overlay this cutex does not read (newer fields?)")
                    })?;
                    if overlay.version != OVERLAY_VERSION {
                        return Err(unsupported(format!(
                            "codex.catalog.overlay: version {} is not supported (this cutex reads {OVERLAY_VERSION})",
                            overlay.version
                        )));
                    }
                    Some(overlay)
                }
            };
            Ok(Some(CatalogPart::Selected { json, overlay }))
        }
        _ => Err(unsupported("codex.catalog.kind: must be local or selected")),
    }
}

fn parse_api_key(credentials: &Value) -> Result<ApiKey, TransferError> {
    if credentials.get("kind").and_then(Value::as_str) != Some("apiKey") {
        return Err(unsupported("credentials.kind: only apiKey credentials can be imported"));
    }
    let key = credentials.get("apiKey").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    if key.is_empty() || key.len() > MAX_API_KEY_BYTES || key.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(TransferError::Invalid(format!(
            "credentials.apiKey: must be 1-{MAX_API_KEY_BYTES} non-whitespace characters"
        )));
    }
    Ok(ApiKey(key.to_string()))
}

fn parse_bundle(bundle: &Value) -> Result<ParsedBundle, TransferError> {
    let bundle = bundle.as_object().ok_or_else(|| TransferError::Invalid("bundle: JSON object required".into()))?;
    if bundle.get("format").and_then(Value::as_str) != Some(BUNDLE_FORMAT) {
        return Err(unsupported(format!("bundle.format: must be {BUNDLE_FORMAT}")));
    }
    match bundle.get("schemaVersion").and_then(Value::as_u64) {
        Some(BUNDLE_SCHEMA_VERSION) => {}
        Some(other) => {
            return Err(unsupported(format!(
                "bundle.schemaVersion {other} is not supported (this cutex reads {BUNDLE_SCHEMA_VERSION})"
            )))
        }
        None => return Err(unsupported("bundle.schemaVersion: required integer")),
    }
    let kind = bundle
        .get("credentialKind")
        .and_then(Value::as_str)
        .and_then(CredentialKind::parse)
        .ok_or_else(|| unsupported("bundle.credentialKind: must be apiKey, chatgpt, claude or none"))?;
    let mut record: Map<String, Value> = match bundle.get("profile") {
        Some(Value::Object(record)) => record.clone(),
        _ => return Err(unsupported("bundle.profile: profile record object required")),
    };
    for key in IDENTITY_KEYS {
        record.remove(*key);
    }
    // Placeholders, replaced by the import.
    record.insert("id".into(), json!(uuid::Uuid::new_v4().to_string()));
    record.insert("name".into(), json!("imported"));
    // Generic message: serde errors may quote values.
    let account: StoredAccount = serde_json::from_value(Value::Object(record)).map_err(|_| {
        unsupported("bundle.profile: not a profile record this cutex reads (unknown CLI kind, runtime or field type)")
    })?;

    let claude = match bundle.get("claudeAccount") {
        Some(Value::Object(claude)) => {
            let label = claude.get("label").and_then(Value::as_str).map(str::to_string);
            let kind = claude.get("kind").and_then(Value::as_str).and_then(ClaudeAccountKind::parse);
            label.zip(kind)
        }
        _ => None,
    };

    let (config, catalog) = if account.cli_kind == CliKind::Codex {
        let codex = bundle.get("codex").filter(|codex| !codex.is_null());
        let config = match codex.and_then(|codex| codex.get("config")) {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => {
                let root = parse_toml_table(text)
                    .map_err(|_| unsupported("codex.config: not a TOML table"))?;
                if root
                    .get("model_catalog_json")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|path| !matches!(path.replace('\\', "/").as_str(), "models.json" | "./models.json"))
                {
                    return Err(unsupported("codex.config: model_catalog_json must not name a host path"));
                }
                Some(text.clone())
            }
            Some(_) => return Err(unsupported("codex.config: string or null required")),
        };
        (config, parse_catalog(codex.and_then(|codex| codex.get("catalog")))?)
    } else {
        (None, None)
    };

    let api_key = match bundle.get("credentials") {
        None | Some(Value::Null) => None,
        Some(credentials) => {
            if account.cli_kind != CliKind::Codex || kind != CredentialKind::ApiKey {
                return Err(TransferError::Invalid(
                    "credentials: only an API-key profile's key can be imported".into(),
                ));
            }
            Some(parse_api_key(credentials)?)
        }
    };
    let dropped = bundle
        .get("dropped")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let source_name = bundle
        .get("source")
        .and_then(|source| source.get("profileName"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(ParsedBundle { account, kind, api_key, claude, config, catalog, dropped, source_name })
}

#[derive(Debug)]
pub struct ImportOutcome {
    pub account: StoredAccount,
    pub credentials_copied: bool,
    pub credential_kind: CredentialKind,
    /// `[{code, ...}]`: what the owner must do or check on this host.
    pub warnings: Vec<Value>,
    pub source_name: Option<String>,
}

fn write_codex_files(account: &StoredAccount, parsed: &ParsedBundle) -> anyhow::Result<()> {
    let files = materialized_account_files(account)?;
    ensure_materialized_account_dir(&files)?;
    let dir = lifecycle::profile_dir(account)?;
    let mut config = parsed.config.clone();
    let mut local_catalog = None;
    match &parsed.catalog {
        Some(CatalogPart::Selected { json, overlay }) => {
            let path = catalog_overlay::write_effective_file(&dir, json)?;
            if let Some(overlay) = overlay {
                write_private_pretty_json_atomic(&dir.join(catalog_overlay::OVERLAY_FILE), overlay, "imported model overlay")?;
            }
            let mut root = config.as_deref().map(parse_toml_table).transpose()?.unwrap_or_default();
            root.insert("model_catalog_json".into(), toml::Value::String(path.to_string_lossy().into_owned()));
            config = Some(toml::to_string_pretty(&root)?);
        }
        Some(CatalogPart::Local(json)) => {
            let mut text = serde_json::to_string_pretty(json)?;
            text.push('\n');
            local_catalog = Some(text);
        }
        None => {}
    }
    let auth = parsed
        .api_key
        .as_ref()
        .map(|key| json!({ "OPENAI_API_KEY": key.0, "tokens": null }).to_string());
    materialize_new_account_files(account, auth.as_deref(), config, local_catalog.as_deref())
}

/// Import `bundle` as a new profile `name` on this host. Never overwrites
/// (`NameConflict`); validates the whole bundle before writing anything.
pub fn import_profile(bundle: &Value, name: &str) -> Result<ImportOutcome, TransferError> {
    let name = lifecycle::validate_profile_name(name).map_err(TransferError::Invalid)?;
    let mut parsed = parse_bundle(bundle)?;
    let _guard = lifecycle::lock();
    let mut store = load_store()?;
    lifecycle::ensure_available(&store, &name, None)?;

    let mut account = parsed.account.clone();
    account.id = uuid::Uuid::new_v4().to_string();
    account.name = name;
    account.email = None;
    account.last_used_at = None;
    account.claude_account = None;
    let mut warnings = Vec::new();

    if account.private_chatgpt_auth_file {
        // Bound to the source host's own native auth.json.
        account.private_chatgpt_auth_file = false;
        warnings.push(json!({ "code": "privateAuthReset" }));
    }
    if let RuntimeConfig::Docker { image, .. } = &account.runtime {
        warnings.push(json!({ "code": "runtimeDockerImage", "image": image }));
    }
    if parsed.dropped.iter().any(|field| field == "proxy") {
        warnings.push(json!({ "code": "proxyCleared" }));
    } else if account.proxy.as_ref().is_some_and(|proxy| proxy.url.is_some()) {
        warnings.push(json!({ "code": "proxyCopied" }));
    }
    let secret_fields: Vec<&String> = parsed.dropped.iter().filter(|field| *field != "proxy").collect();
    if !secret_fields.is_empty() {
        warnings.push(json!({ "code": "secretFieldsCleared", "fields": secret_fields }));
    }

    if account.cli_kind == CliKind::Claude {
        let registry = claude_accounts::load()?;
        let matched = parsed.claude.as_ref().and_then(|(label, kind)| {
            registry.accounts.iter().find(|candidate| candidate.label == *label && candidate.kind == *kind)
        });
        let (target_id, target_label, logged_in) = match matched {
            Some(found) => {
                account.claude_account = Some(found.id.clone());
                (found.id.clone(), Some(found.label.clone()), found.last_status.as_ref().map(|s| s.logged_in))
            }
            None => (
                claude_accounts::DEFAULT_ACCOUNT_ID.to_string(),
                None,
                registry.default_status.as_ref().map(|s| s.logged_in),
            ),
        };
        warnings.push(json!({
            "code": "claudeAccountRebound",
            "sourceLabel": parsed.claude.as_ref().map(|(label, _)| label),
            "sourceKind": parsed.claude.as_ref().map(|(_, kind)| kind.as_str()),
            "matched": parsed.claude.is_none() || matched.is_some(),
            "account": target_id,
            "accountLabel": target_label,
            "loggedIn": logged_in,
        }));
    }

    let credentials_copied = parsed.api_key.is_some();
    match parsed.kind {
        CredentialKind::Chatgpt => warnings.push(json!({ "code": "needsLogin", "credential": "chatgpt" })),
        CredentialKind::ApiKey if !credentials_copied => {
            warnings.push(json!({ "code": "needsLogin", "credential": "apiKey" }))
        }
        _ => {}
    }

    if account.cli_kind == CliKind::Codex {
        if let Err(error) = write_codex_files(&account, &parsed) {
            lifecycle::remove_profile_dir(&account);
            return Err(error.into());
        }
    }
    // Drop the key from memory as soon as it is on disk.
    parsed.api_key = None;
    store.accounts.push(account.clone());
    if store.active_account_id.is_none() {
        store.active_account_id = Some(account.id.clone());
    }
    if let Err(error) = save_store(&store) {
        lifecycle::remove_profile_dir(&account);
        return Err(error.into());
    }
    Ok(ImportOutcome {
        account,
        credentials_copied,
        credential_kind: parsed.kind,
        warnings,
        source_name: parsed.source_name,
    })
}

/// Append one audit line (`<runtime>/profile-transfer/audit.jsonl`): which
/// profile, whether credentials travelled, the actor and whether the
/// connection was loopback. Never values.
pub fn audit(action: &str, details: Value) {
    match crate::config::paths::runtime_dir() {
        Ok(dir) => crate::management::users::audit(&dir.join(AUDIT_DIR), action, details),
        Err(error) => eprintln!("profile transfer audit: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::lifecycle::tests::TestHome;
    use crate::profiles::lifecycle::{create_profile, NewProfile, NewProfileKind};
    use crate::profiles::providers::find_preset;

    const KEY: &str = "transfer-test-placeholder-key";

    fn api_key_profile(name: &str, preset: &str, key: Option<&str>) -> StoredAccount {
        create_profile(&NewProfile {
            name: name.into(),
            kind: NewProfileKind::ApiKey,
            preset: find_preset(preset),
            base_url: None,
            model: None,
            api_key: key.map(|key| ApiKey(key.into())),
        })
        .unwrap()
    }

    fn remove(name: &str) {
        let mut store = load_store().unwrap();
        let account = find(&store, name).unwrap().clone();
        store.accounts.retain(|candidate| candidate.id != account.id);
        save_store(&store).unwrap();
        lifecycle::remove_profile_dir(&account);
    }

    #[test]
    fn api_key_round_trip_with_and_without_credentials() {
        let _home = TestHome::new();
        let source = api_key_profile("GLM", "glm", Some(KEY));
        assert_eq!(credential_kind(&source), CredentialKind::ApiKey);

        // Without credentials: no key anywhere in the bundle.
        let plain = export_profile("GLM", false).unwrap();
        assert!(!plain.credentials_included);
        assert!(!plain.bundle.to_string().contains(KEY));
        assert_eq!(plain.bundle["credentialKind"], "apiKey");
        assert!(plain.bundle["profile"].get("id").is_none() && plain.bundle["profile"].get("name").is_none());
        assert_eq!(plain.bundle["codex"]["catalog"]["kind"], "local");

        let with_key = export_profile(&source.id, true).unwrap();
        assert!(with_key.credentials_included);
        assert_eq!(with_key.bundle["credentials"]["apiKey"], KEY);
        assert!(!format!("{with_key:?}").is_empty());

        // "Another host": the same home after the source is gone.
        remove("GLM");
        let imported = import_profile(&with_key.bundle, "GLM").unwrap();
        assert!(imported.credentials_copied);
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
        assert_ne!(imported.account.id, source.id);
        assert_eq!(imported.account.options.quota, source.options.quota);
        assert_eq!(imported.account.plan_type, source.plan_type);
        let files = materialized_account_files(&imported.account).unwrap();
        let auth: Value = serde_json::from_str(&std::fs::read_to_string(&files.auth_path).unwrap()).unwrap();
        assert_eq!(auth["OPENAI_API_KEY"], KEY);
        let config = std::fs::read_to_string(&files.config_path).unwrap();
        assert!(config.contains("model_provider = \"zai\"") && config.contains("glm-5.3"), "{config}");
        assert!(files.model_catalog_path.exists());
        assert_eq!(credential_kind(&imported.account), CredentialKind::ApiKey);

        // Without the key: the copy needs the key entered again.
        let copy = import_profile(&plain.bundle, "GLM 2").unwrap();
        assert!(!copy.credentials_copied);
        assert_eq!(copy.warnings, [json!({ "code": "needsLogin", "credential": "apiKey" })]);
        assert!(!materialized_account_files(&copy.account).unwrap().auth_path.exists());
    }

    #[test]
    fn import_never_overwrites_and_rejects_unknown_bundles() {
        let _home = TestHome::new();
        let source = api_key_profile("ds", "deepseek", Some(KEY));
        let export = export_profile("ds", false).unwrap();
        assert!(matches!(import_profile(&export.bundle, "ds"), Err(TransferError::NameConflict(_))));
        assert!(matches!(import_profile(&export.bundle, &source.id), Err(TransferError::NameConflict(_))));
        assert!(matches!(import_profile(&export.bundle, "a/b"), Err(TransferError::Invalid(_))));
        assert!(matches!(export_profile("nope", false), Err(TransferError::NotFound)));

        let mut newer = export.bundle.clone();
        newer["schemaVersion"] = json!(2);
        assert!(matches!(import_profile(&newer, "x"), Err(TransferError::Unsupported(_))));
        let mut wrong = export.bundle.clone();
        wrong["format"] = json!("something-else");
        assert!(matches!(import_profile(&wrong, "x"), Err(TransferError::Unsupported(_))));
        let mut kind = export.bundle.clone();
        kind["profile"]["cli_kind"] = json!("gemini");
        assert!(matches!(import_profile(&kind, "x"), Err(TransferError::Unsupported(_))));
        let mut host_path = export.bundle.clone();
        host_path["codex"]["config"] = json!("model_catalog_json = \"/home/a/.cutex/profiles/x/models-1.json\"\n");
        assert!(matches!(import_profile(&host_path, "x"), Err(TransferError::Unsupported(_))));
        // Unknown fields are ignored (additive schema).
        let mut extra = export.bundle.clone();
        extra["futureField"] = json!({ "a": 1 });
        extra["profile"]["future_option"] = json!(true);
        import_profile(&extra, "ds copy").expect("unknown fields ignored");
        // Nothing half-written by the refused imports.
        let store = load_store().unwrap();
        assert_eq!(store.accounts.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["ds", "ds copy"]);
    }

    #[test]
    fn credentials_in_a_bundle_are_validated_without_echo() {
        let _home = TestHome::new();
        api_key_profile("k", "moonshot", Some(KEY));
        let mut bundle = export_profile("k", true).unwrap().bundle;
        bundle["credentials"]["apiKey"] = json!("sk-has space-secret");
        let error = import_profile(&bundle, "k2").unwrap_err().to_string();
        assert!(!error.contains("secret"), "{error}");
        bundle["credentials"] = json!({ "kind": "chatgpt", "tokens": { "refresh_token": "rt-secret" } });
        let error = import_profile(&bundle, "k2").unwrap_err().to_string();
        assert!(!error.contains("rt-secret"), "{error}");
        // A malformed record never echoes values either.
        let mut bundle = export_profile("k", true).unwrap().bundle;
        bundle["profile"]["runtime"] = json!({ "kind": format!("{KEY}") });
        let error = import_profile(&bundle, "k2").unwrap_err().to_string();
        assert!(!error.contains(KEY), "{error}");
    }

    #[test]
    fn chatgpt_login_never_travels_and_host_settings_reset() {
        let _home = TestHome::new();
        let source = create_profile(&NewProfile {
            name: "work".into(),
            kind: NewProfileKind::Chatgpt,
            preset: None,
            base_url: None,
            model: Some("gpt-5.5".into()),
            api_key: None,
        })
        .unwrap();
        let files = materialized_account_files(&source).unwrap();
        std::fs::write(
            &files.auth_path,
            r#"{"OPENAI_API_KEY":null,"tokens":{"id_token":"id-secret","access_token":"at-secret","refresh_token":"rt-secret"}}"#,
        )
        .unwrap();
        let mut store = load_store().unwrap();
        let stored = store.accounts.iter_mut().find(|a| a.id == source.id).unwrap();
        stored.email = Some("owner@example.test".into());
        stored.plan_type = Some("pro".into());
        stored.private_chatgpt_auth_file = true;
        stored.runtime = RuntimeConfig::Docker { image: "cutex-dev".into(), user_name: None };
        stored.proxy = Some(crate::profiles::model::ProxyConfig {
            enabled: true,
            url: Some("http://user:proxy-secret@127.0.0.1:7890".into()),
            no_proxy: None,
            force_http_transport: true,
        });
        save_store(&store).unwrap();
        let mut raw = std::fs::read_to_string(&files.config_path).unwrap();
        raw.push_str("\n[shell_environment_policy.set]\nTOKEN = \"env-secret\"\n");
        std::fs::write(&files.config_path, raw).unwrap();

        let export = export_profile("work", true).unwrap();
        assert_eq!(export.credential_kind, CredentialKind::Chatgpt);
        assert!(!export.credentials_included, "a ChatGPT login never travels");
        let text = export.bundle.to_string();
        for secret in ["id-secret", "at-secret", "rt-secret", "owner@example.test", "\"pro\""] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let plain = export_profile("work", false).unwrap().bundle.to_string();
        assert!(!plain.contains("proxy-secret") && !plain.contains("env-secret"), "{plain}");

        let imported = import_profile(&export_profile("work", false).unwrap().bundle, "work 2").unwrap();
        let codes: Vec<_> = imported.warnings.iter().map(|w| w["code"].as_str().unwrap().to_string()).collect();
        assert_eq!(
            codes,
            ["privateAuthReset", "runtimeDockerImage", "proxyCleared", "secretFieldsCleared", "needsLogin"]
        );
        assert!(!imported.account.private_chatgpt_auth_file);
        assert!(imported.account.proxy.is_none() && imported.account.email.is_none());
        assert!(!materialized_account_files(&imported.account).unwrap().auth_path.exists());
        let config = std::fs::read_to_string(materialized_account_files(&imported.account).unwrap().config_path).unwrap();
        assert!(config.contains("gpt-5.5") && !config.contains("env-secret"), "{config}");
    }

    #[test]
    fn selected_catalog_and_overlay_move_with_the_copy() {
        let _home = TestHome::new();
        let source = api_key_profile("x", "xai", Some(KEY));
        let dir = lifecycle::profile_dir(&source).unwrap();
        let catalog = json!({ "models": [{ "slug": "grok-test", "display_name": "Grok test" }] });
        let path = catalog_overlay::write_effective_file(&dir, &catalog).unwrap();
        let files = materialized_account_files(&source).unwrap();
        let mut root = parse_toml_table(&std::fs::read_to_string(&files.config_path).unwrap()).unwrap();
        root.insert("model_catalog_json".into(), toml::Value::String(path.to_string_lossy().into_owned()));
        std::fs::write(&files.config_path, toml::to_string_pretty(&root).unwrap()).unwrap();
        let overlay = Overlay { version: OVERLAY_VERSION, migrated_from: None, entries: Vec::new() };
        write_private_pretty_json_atomic(&dir.join(catalog_overlay::OVERLAY_FILE), &overlay, "test overlay").unwrap();

        let export = export_profile("x", false).unwrap();
        assert_eq!(export.bundle["codex"]["catalog"]["kind"], "selected");
        let config = export.bundle["codex"]["config"].as_str().unwrap();
        assert!(!config.contains("model_catalog_json"), "no host path: {config}");

        let imported = import_profile(&export.bundle, "x copy").unwrap();
        let state = crate::profiles::catalog_api::load(&imported.account).unwrap();
        assert_eq!(state.catalog.unwrap(), catalog);
        assert!(state.path.unwrap().starts_with(lifecycle::profile_dir(&imported.account).unwrap()));
        assert_eq!(catalog_overlay::load(&imported.account).unwrap(), Some(overlay));
    }

    #[test]
    fn claude_profiles_rebind_to_the_target_account() {
        let _home = TestHome::new();
        create_profile(&NewProfile {
            name: "claude".into(),
            kind: NewProfileKind::Claude,
            preset: None,
            base_url: None,
            model: Some("opus".into()),
            api_key: None,
        })
        .unwrap();
        let mut bundle = export_profile("claude", true).unwrap().bundle;
        assert_eq!(bundle["credentialKind"], "claude");
        assert!(bundle["credentials"].is_null() && bundle["codex"].is_null());
        // A source account "work" (subscription) that this host lacks.
        bundle["claudeAccount"] = json!({ "label": "work", "kind": "subscription" });
        let imported = import_profile(&bundle, "claude 2").unwrap();
        assert_eq!(imported.account.claude_account, None);
        assert_eq!(imported.account.options.claude.model, "opus");
        let warning = &imported.warnings[0];
        assert_eq!(warning["code"], "claudeAccountRebound");
        assert_eq!((warning["matched"].as_bool(), warning["account"].as_str()), (Some(false), Some("default")));
    }
}
