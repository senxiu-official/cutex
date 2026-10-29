//! r265 Profile runtime variants, credential view, compatibility matrix and
//! the frozen launch binding of new agents
//! (docs/development/r265-profile-variants.md).
//!
//! The primary variant is the profile's legacy runtime (`cli_kind`) and is
//! computed, never stored; only secondary variants and a non-primary default
//! are persisted (top-level additive fields of `accounts.json`, ignored and
//! dropped by older binaries). Nothing here reads or returns a key value;
//! see [`super::stored_keys`].

use std::collections::BTreeMap;

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::model::{CliKind, StoredAccount};

pub const FEATURE: &str = "profile-runtime-variants";
pub const MAX_VARIANTS: usize = 4;

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Stored per-profile variant state, flattened into [`StoredAccount`]. Every
/// field is lenient on read (unknown values are kept as strings), so a
/// future value never makes the accounts store unreadable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantState {
    /// Secondary variants only (the primary is computed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_variants: Vec<StoredVariant>,
    /// Absent = the primary variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_variant: Option<String>,
    /// DSH profiles: `stored` | `env`. Absent = legacy (`env` with
    /// `options.dsh.api_key_env`). Codex / Claude profiles: absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_source: Option<String>,
    /// Increments on every stored-key write (DSH stored key, Codex API key).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub credential_version: u64,
    /// r266 Kimi-kind profiles: the Kimi account (`profiles::kimi_accounts`)
    /// new agents run on. Absent = no credential (`credential_missing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kimi_account: Option<String>,
    /// r266 Kimi-kind profiles: the primary variant's defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kimi_defaults: Option<VariantDefaults>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredVariant {
    pub id: String,
    pub runtime: String,
    #[serde(default)]
    pub defaults: VariantDefaults,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub id: String,
    pub runtime: CliKind,
    pub primary: bool,
    /// Secondary variants only; the primary's defaults are the profile's
    /// existing model settings.
    pub defaults: VariantDefaults,
}

/// Credential source of a profile (wire names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Stored,
    Env,
    ChatgptLogin,
    ClaudeAccount,
    /// r266 Kimi-kind profiles (a Kimi subscription account).
    KimiAccount,
    None,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Env => "env",
            Self::ChatgptLogin => "chatgptLogin",
            Self::ClaudeAccount => "claudeAccount",
            Self::KimiAccount => "kimiAccount",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Supported,
    ComponentNotInstalled,
    Unsupported,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::ComponentNotInstalled => "componentNotInstalled",
            Self::Unsupported => "unsupported",
        }
    }
}

pub fn runtime_name(kind: &CliKind) -> &'static str {
    match kind {
        CliKind::Codex => "codex",
        CliKind::Claude => "claude",
        CliKind::Dsh => "dsh",
        CliKind::Kimi => "kimi",
    }
}

/// Every runtime, in `runtimeCompatibility` order.
pub const RUNTIMES: [CliKind; 4] = [CliKind::Codex, CliKind::Claude, CliKind::Dsh, CliKind::Kimi];

fn parse_runtime(value: &str) -> Option<CliKind> {
    match value {
        "codex" => Some(CliKind::Codex),
        "claude" => Some(CliKind::Claude),
        "dsh" => Some(CliKind::Dsh),
        "kimi" => Some(CliKind::Kimi),
        _ => None,
    }
}

pub fn validate_variant_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "variant id must be 1-32 characters of a-z, 0-9 and -"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Provider facts
// ---------------------------------------------------------------------------

pub fn is_api_key_profile(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex
        && account.source.as_deref() == Some("api-key")
        && !account.private_chatgpt_auth_file
}

/// A Codex API-key profile whose provider id is DeepSeek, on any address
/// (owner decision 2026-10-28: official base URLs are only defaults; a relay
/// keeps the provider's runtimes). Names and model ids never count.
pub fn is_deepseek_api_key(account: &StoredAccount) -> bool {
    super::interface_urls::is_deepseek_api_key(account)
}

fn claude_api_account(account: &StoredAccount) -> Option<crate::runtime::claude_accounts::ClaudeAccount> {
    let id = account.claude_account.as_deref()?;
    let registry = crate::runtime::claude_accounts::load_cached();
    registry
        .find(id)
        .filter(|found| found.kind == crate::runtime::claude_accounts::ClaudeAccountKind::Api)
        .cloned()
}

/// `providerInterfaces` (profile-relay-runtimes rows: address, official,
/// relayAccepted; `profiles::interface_urls`).
pub fn provider_interfaces(account: &StoredAccount) -> Value {
    super::interface_urls::profile_view(account)
}

// ---------------------------------------------------------------------------
// Credential view (never the value)
// ---------------------------------------------------------------------------

pub fn credential_source(account: &StoredAccount) -> Source {
    match account.cli_kind {
        CliKind::Dsh => match account.variants.credential_source.as_deref() {
            Some("stored") => Source::Stored,
            Some("env") | None => Source::Env,
            Some(_) => Source::None,
        },
        CliKind::Claude => Source::ClaudeAccount,
        CliKind::Kimi if account.variants.kimi_account.is_some() => Source::KimiAccount,
        CliKind::Kimi => Source::None,
        CliKind::Codex if is_api_key_profile(account) => Source::Stored,
        CliKind::Codex => Source::ChatgptLogin,
    }
}

fn host_env_present(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

/// Version of the profile's stored credential: the first write of a key is
/// version 1 and each later write (or removal) adds one. A key present with
/// no recorded version (written before r265 counted, or by an r265 build that
/// did not count the create write) reads as version 1; nothing is rewritten.
pub fn credential_version(account: &StoredAccount) -> u64 {
    // A DSH stored key's version is written with the key (one atomic file
    // write); the profile store's copy may lag it after a crash in between.
    let version = match account.cli_kind {
        CliKind::Dsh => account.variants.credential_version.max(super::stored_keys::stored_version(&account.id).unwrap_or(0)),
        _ => account.variants.credential_version,
    };
    if version > 0 || credential_source(account) != Source::Stored {
        return version;
    }
    let present = match account.cli_kind {
        CliKind::Dsh => super::stored_keys::present(&account.id),
        _ => super::inspect::account_uses_api_key_auth(account),
    };
    u64::from(present)
}

pub fn credential_view(account: &StoredAccount) -> Value {
    let mut view = credential_view_inner(account);
    // r266 additive key on every credential view.
    view["kimiAccountId"] = json!(account.variants.kimi_account.as_deref().filter(|_| account.cli_kind == CliKind::Kimi));
    view
}

fn credential_view_inner(account: &StoredAccount) -> Value {
    let source = credential_source(account);
    let version = credential_version(account);
    match (source, &account.cli_kind) {
        (Source::Stored, CliKind::Dsh) => json!({
            "source": "stored", "present": super::stored_keys::present(&account.id), "envName": null,
            "claudeAccountId": null, "version": version, "writable": true,
        }),
        (Source::Env, _) => json!({
            "source": "env", "present": host_env_present(&account.options.dsh.api_key_env),
            "envName": account.options.dsh.api_key_env, "claudeAccountId": null, "version": null, "writable": true,
        }),
        (Source::Stored, _) => json!({
            "source": "stored", "present": super::inspect::account_uses_api_key_auth(account), "envName": null,
            "claudeAccountId": null, "version": version, "writable": true,
        }),
        (Source::ClaudeAccount, _) => {
            let id = account.claude_account.clone().unwrap_or_else(|| crate::runtime::claude_accounts::DEFAULT_ACCOUNT_ID.to_string());
            let present = claude_api_account(account).map(|found| crate::runtime::claude_accounts::api_key_present(&found.id));
            json!({ "source": "claudeAccount", "present": present, "envName": null, "claudeAccountId": id, "version": null, "writable": false })
        }
        (Source::KimiAccount, _) => {
            let present = account
                .variants
                .kimi_account
                .as_deref()
                .and_then(|id| super::kimi_accounts::account_credential_state(id).ok())
                .is_some_and(|state| state.usable());
            json!({ "source": "kimiAccount", "present": present, "envName": null, "claudeAccountId": null, "version": null, "writable": false })
        }
        (Source::ChatgptLogin, _) => json!({
            "source": "chatgptLogin", "present": super::inspect::account_uses_chatgpt_auth(account),
            "envName": null, "claudeAccountId": null, "version": null, "writable": false,
        }),
        (Source::None, _) => json!({
            "source": "none", "present": false, "envName": null, "claudeAccountId": null, "version": null, "writable": true,
        }),
    }
}

// ---------------------------------------------------------------------------
// Compatibility matrix
// ---------------------------------------------------------------------------

fn dsh_installed() -> (Status, Option<&'static str>) {
    if !cfg!(any(windows, target_os = "linux")) {
        return (Status::ComponentNotInstalled, Some("platformUnsupported"));
    }
    if crate::components::dsh_runtime::advertised() {
        (Status::Supported, None)
    } else {
        (Status::ComponentNotInstalled, Some("componentNotInstalled"))
    }
}

fn claude_installed() -> (Status, Option<&'static str>) {
    if crate::runtime::claude::deployment_path().is_ok_and(|path| path.is_file()) {
        (Status::Supported, None)
    } else {
        (Status::ComponentNotInstalled, Some("componentNotInstalled"))
    }
}

/// `(status, reason)` of `runtime` for this profile's provider + credential.
pub fn compatibility(account: &StoredAccount, runtime: &CliKind) -> (Status, Option<&'static str>) {
    use CliKind::*;
    let unsupported = |reason| (Status::Unsupported, Some(reason));
    match (&account.cli_kind, runtime) {
        (Codex, Codex) | (Claude, Claude) | (Dsh, Dsh) | (Kimi, Kimi) => match runtime {
            Dsh => dsh_installed(),
            Claude => claude_installed(),
            Codex => (Status::Supported, None),
            Kimi => super::kimi::component_status(),
        },
        (Kimi, _) => unsupported("kimiAccountKimiOnly"),
        (Codex, _) if !is_api_key_profile(account) => unsupported("chatgptLoginCodexOnly"),
        (Codex, Kimi) if super::kimi::is_native_api_key(account) => super::kimi::component_status(),
        (Codex, Kimi) => unsupported("providerNotKimiNative"),
        (Codex, Claude) if super::kimi::is_native_api_key(account) => unsupported("anthropicRouteNotYetSupported"),
        (Codex, Dsh) if is_deepseek_api_key(account) => dsh_installed(),
        (Codex, Dsh) => unsupported("providerNotDeepseekNative"),
        (Codex, Claude) if is_deepseek_api_key(account) => unsupported("anthropicRouteNotYetSupported"),
        (Codex, Claude) => unsupported("anthropicRouteRequired"),
        (Claude, _) => unsupported("claudeCredentialClaudeOnly"),
        (Dsh, Codex) => unsupported("dshProfileCodexVariantNotYetSupported"),
        (Dsh, Claude) => unsupported("anthropicRouteNotYetSupported"),
        (Dsh, Kimi) => unsupported("providerNotKimiNative"),
    }
}

/// `runtimeCompatibility` of a new API-key (Codex) profile created from an
/// API-key preset (`GET /v2/profile-presets` items, additive): the same rows
/// `compatibility` gives that profile, before it exists. Only the provider id
/// counts, never the base URL (a relay keeps the provider's runtimes).
pub fn preset_compatibility_view(provider_id: &str, base_url: Option<&str>) -> Value {
    let _ = base_url; // The address never decides the runtimes (relays keep them).
    let deepseek = super::deepseek::is_deepseek_provider(provider_id);
    let kimi = super::kimi::is_kimi_provider(provider_id);
    let unsupported = |reason: &'static str| (Status::Unsupported, Some(reason));
    Value::Array(
        RUNTIMES
            .iter()
            .map(|runtime| {
                let (status, reason) = match runtime {
                    CliKind::Codex => (Status::Supported, None),
                    CliKind::Claude if deepseek || kimi => unsupported("anthropicRouteNotYetSupported"),
                    CliKind::Claude => unsupported("anthropicRouteRequired"),
                    CliKind::Dsh if deepseek => dsh_installed(),
                    CliKind::Dsh => unsupported("providerNotDeepseekNative"),
                    CliKind::Kimi if kimi => super::kimi::component_status(),
                    CliKind::Kimi => unsupported("providerNotKimiNative"),
                };
                json!({ "runtime": runtime_name(runtime), "status": status.as_str(), "reason": reason, "variantPresent": false })
            })
            .collect(),
    )
}

pub fn compatibility_view(account: &StoredAccount) -> Value {
    let present = variants(account);
    Value::Array(
        RUNTIMES
            .iter()
            .map(|runtime| {
                let (status, reason) = compatibility(account, runtime);
                json!({
                    "runtime": runtime_name(runtime),
                    "status": status.as_str(),
                    "reason": reason,
                    "variantPresent": present.iter().any(|variant| &variant.runtime == runtime),
                })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Effective variants
// ---------------------------------------------------------------------------

pub fn primary_id(account: &StoredAccount) -> &'static str {
    runtime_name(&account.cli_kind)
}

pub fn variants(account: &StoredAccount) -> Vec<Variant> {
    let mut out = vec![Variant {
        id: primary_id(account).to_string(),
        runtime: account.cli_kind.clone(),
        primary: true,
        defaults: VariantDefaults::default(),
    }];
    for stored in &account.variants.runtime_variants {
        // An unknown runtime from a newer binary is not offered here.
        if let Some(runtime) = parse_runtime(&stored.runtime) {
            if out.iter().all(|variant| variant.id != stored.id) {
                out.push(Variant { id: stored.id.clone(), runtime, primary: false, defaults: stored.defaults.clone() });
            }
        }
    }
    out
}

pub fn default_variant_id(account: &StoredAccount) -> String {
    let all = variants(account);
    account
        .variants
        .default_variant
        .as_ref()
        .filter(|id| all.iter().any(|variant| &variant.id == *id))
        .cloned()
        .unwrap_or_else(|| primary_id(account).to_string())
}

/// Bound sessions per `(profileId, variantId)`; legacy sessions (no binding)
/// count for the primary variant of the profile named in their record.
#[derive(Debug, Default)]
pub struct Usage {
    by_variant: BTreeMap<(String, String), Vec<String>>,
    legacy_by_name: BTreeMap<String, usize>,
}

impl Usage {
    pub fn load() -> Self {
        let mut usage = Self::default();
        let Ok(store) = crate::session::store::load_cutex_session_store_shared() else {
            return usage;
        };
        for (id, record) in &store.sessions {
            if record.is_retired() {
                continue;
            }
            match &record.profile_binding {
                Some(binding) => usage
                    .by_variant
                    .entry((binding.profile_id.clone(), binding.variant_id.clone()))
                    .or_default()
                    .push(id.clone()),
                None => {
                    if let Some(name) = &record.profile {
                        *usage.legacy_by_name.entry(name.clone()).or_default() += 1;
                    }
                }
            }
        }
        for sessions in usage.by_variant.values_mut() {
            sessions.sort();
        }
        usage
    }

    pub fn sessions(&self, profile_id: &str, variant_id: &str) -> &[String] {
        self.by_variant
            .get(&(profile_id.to_string(), variant_id.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn count(&self, account: &StoredAccount, variant: &Variant) -> usize {
        let bound = self.sessions(&account.id, &variant.id).len();
        if variant.primary {
            bound + self.legacy_by_name.get(&account.name).copied().unwrap_or(0)
        } else {
            bound
        }
    }
}

/// Adds the r265 fields to a profile item built by `settings_api::profile_item`.
pub fn augment_item(item: &mut Value, account: &StoredAccount, usage: &Usage) {
    let rows = variants(account)
        .iter()
        .map(|variant| {
            let defaults = if variant.primary {
                json!({ "model": item.get("model").cloned().unwrap_or(Value::Null),
                        "reasoningEffort": item.get("modelReasoningEffort").cloned().unwrap_or(Value::Null) })
            } else {
                json!({ "model": variant.defaults.model.clone().or_else(|| secondary_default_model(account, &variant.runtime)),
                        "reasoningEffort": variant.defaults.reasoning_effort })
            };
            json!({
                "id": variant.id,
                "runtime": runtime_name(&variant.runtime),
                "primary": variant.primary,
                "defaults": defaults,
                "inUse": usage.count(account, variant),
                // DSH: its pinned catalog (r265 additive). Other runtimes: null,
                // their list is the profile's models resource.
                "models": match variant.runtime {
                    CliKind::Dsh => super::dsh::catalog_wire(),
                    // r266: a Kimi account's provisioned catalog; API-key Kimi variants: null.
                    CliKind::Kimi if account.cli_kind == CliKind::Kimi => super::kimi::profile_catalog(account).wire(),
                    _ => Value::Null,
                },
            })
        })
        .collect::<Vec<_>>();
    item["providerInterfaces"] = provider_interfaces(account);
    item["credential"] = credential_view(account);
    item["runtimeVariants"] = Value::Array(rows);
    item["defaultVariant"] = json!(default_variant_id(account));
    item["runtimeCompatibility"] = compatibility_view(account);
    item["kimiAccount"] = json!(account.variants.kimi_account.as_deref().filter(|_| account.cli_kind == CliKind::Kimi));
}

fn secondary_default_model(account: &StoredAccount, runtime: &CliKind) -> Option<String> {
    match runtime {
        CliKind::Dsh => Some(super::dsh::DEFAULT_MODEL.to_string()),
        CliKind::Kimi => super::kimi::api_key_default_model(account),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Variant mutations (caller holds the profile lifecycle lock)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl Refusal {
    fn new(status: u16, code: &'static str, message: impl Into<String>, details: Value) -> Self {
        Self { status, code, message: message.into(), details }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message, json!({}))
    }
    fn internal(error: anyhow::Error) -> Self {
        Self::new(500, "profile_store_failed", format!("{error:#}"), json!({}))
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub fn validate_defaults(runtime: &CliKind, defaults: &VariantDefaults) -> anyhow::Result<()> {
    match runtime {
        CliKind::Dsh => {
            if let Some(model) = &defaults.model {
                super::dsh::validate_catalog_model(model)?;
            }
            super::dsh::DshOptions {
                model: defaults.model.clone().unwrap_or_else(|| super::dsh::DEFAULT_MODEL.to_string()),
                reasoning_effort: defaults.reasoning_effort.clone(),
                api_key_env: super::dsh::DEFAULT_API_KEY_ENV.into(),
            }
            .validate()
        }
        CliKind::Kimi => super::kimi::validate_defaults(defaults, None),
        _ => anyhow::bail!("only DSH and Kimi secondary variants exist"),
    }
}

fn load_for_update(profile: &str) -> Result<(super::model::AccountsStore, usize), Refusal> {
    let store = super::settings_api::load_store().map_err(Refusal::internal)?;
    let index = store
        .accounts
        .iter()
        .position(|account| account.id == profile || account.name == profile)
        .ok_or_else(|| Refusal::new(404, "profile_not_found", format!("no profile {profile}"), json!({ "profile": profile })))?;
    Ok((store, index))
}

fn save(store: &super::model::AccountsStore) -> Result<(), Refusal> {
    super::store::save_store(store).map_err(Refusal::internal)
}

pub fn add_variant(profile: &str, runtime: &str, id: Option<&str>, defaults: VariantDefaults) -> Result<StoredAccount, Refusal> {
    add_variant_with_address(profile, runtime, id, defaults, None)
}

/// The interface whose address a launch of `runtime` on `account` reads
/// (profile-relay-runtimes); `None`: the runtime's own config (Codex, Claude,
/// Kimi accounts).
pub fn launch_interface(account: &StoredAccount, runtime: &CliKind) -> Option<super::interface_urls::InterfaceKind> {
    use super::interface_urls::InterfaceKind;
    match runtime {
        CliKind::Dsh => Some(InterfaceKind::DeepseekNative),
        CliKind::Kimi if account.cli_kind != CliKind::Kimi => Some(InterfaceKind::KimiNative),
        _ => None,
    }
}

/// `variant_base_url_missing` / `relay_runtime_unavailable` when a launch of
/// `runtime` on `account` has no usable address.
fn require_address(account: &StoredAccount, runtime: &CliKind, variant_id: &str) -> Result<(), Refusal> {
    let Some(kind) = launch_interface(account, runtime) else { return Ok(()) };
    super::interface_urls::require(account, kind).map(drop).map_err(|code| {
        let message = if code == super::interface_urls::BASE_URL_MISSING {
            format!("profile {} uses a relay base URL; set the {} address for runtime {} (interfaceBaseUrls.{})",
                account.name, kind.as_str(), runtime_name(runtime), kind.as_str())
        } else {
            format!("runtime {} on this host cannot use profile {}'s base URL", runtime_name(runtime), account.name)
        };
        Refusal::new(409, code, message, json!({ "profile": account.id, "variantId": variant_id, "runtime": runtime_name(runtime), "interface": kind.as_str() }))
    })
}

/// [`add_variant`] with the variant's own address (`baseUrl` of
/// `POST /v2/profiles/{p}/variants`; DSH only: Kimi Code shares the
/// profile's base URL). It is stored as the profile's `deepseekNative`
/// interface address. Adding a DSH variant to a profile whose main address
/// is a relay requires one (`variant_base_url_missing`): it is never guessed.
pub fn add_variant_with_address(profile: &str, runtime: &str, id: Option<&str>, defaults: VariantDefaults,
    base_url: Option<String>) -> Result<StoredAccount, Refusal> {
    let (mut store, index) = load_for_update(profile)?;
    let account = &mut store.accounts[index];
    let kind = parse_runtime(runtime).ok_or_else(|| Refusal::invalid("runtime must be codex, claude, dsh or kimi"))?;
    let id = id.unwrap_or(runtime).to_string();
    validate_variant_id(&id).map_err(|error| Refusal::invalid(format!("id: {error}")))?;
    let existing = variants(account);
    if existing.iter().any(|variant| variant.id == id || variant.runtime == kind) {
        return Err(Refusal::new(409, "variant_exists", "this profile already has a variant with that id or runtime",
            json!({ "profile": account.id, "variantId": id, "runtime": runtime })));
    }
    let (status, reason) = compatibility(account, &kind);
    if status == Status::Unsupported || !matches!(kind, CliKind::Dsh | CliKind::Kimi) {
        return Err(Refusal::new(409, "variant_runtime_unsupported", "this profile's provider and credential cannot run that runtime",
            json!({ "profile": account.id, "runtime": runtime, "status": status.as_str(), "reason": reason.or(Some("notInR265")) })));
    }
    if existing.len() >= MAX_VARIANTS {
        return Err(Refusal::invalid(format!("at most {MAX_VARIANTS} variants per profile")));
    }
    validate_defaults(&kind, &defaults).map_err(|error| Refusal::invalid(format!("defaults: {error:#}")))?;
    if let Some(url) = base_url {
        let interface = launch_interface(account, &kind)
            .filter(|interface| super::interface_urls::stored_kinds_for(account).contains(interface))
            .ok_or_else(|| Refusal::invalid(format!("baseUrl: runtime {runtime} uses the profile's base URL (providerBaseUrl)")))?;
        let patch = BTreeMap::from([(interface.as_str().to_string(), Some(url))]);
        super::interface_urls::apply_patch(account, &patch).map_err(|error| Refusal::invalid(format!("baseUrl: {error:#}")))?;
    }
    require_address(account, &kind, &id)?;
    account.variants.runtime_variants.push(StoredVariant {
        id,
        runtime: runtime.to_string(),
        defaults,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
    });
    let account = account.clone();
    save(&store)?;
    Ok(account)
}

/// Secondary variants only; the caller routes the primary to the profile PATCH.
pub fn patch_variant(profile: &str, variant_id: &str, model: crate::config::global_settings::ConfigValueUpdate<String>,
    effort: crate::config::global_settings::ConfigValueUpdate<String>) -> Result<StoredAccount, Refusal> {
    use crate::config::global_settings::ConfigValueUpdate as U;
    let (mut store, index) = load_for_update(profile)?;
    let account = &mut store.accounts[index];
    let profile_id = account.id.clone();
    let stored = account
        .variants
        .runtime_variants
        .iter_mut()
        .find(|variant| variant.id == variant_id)
        .ok_or_else(|| unknown_variant(&profile_id, variant_id))?;
    let mut defaults = stored.defaults.clone();
    match model { U::Set(value) => defaults.model = Some(value), U::Clear => defaults.model = None, U::Unchanged => {} }
    match effort { U::Set(value) => defaults.reasoning_effort = Some(value), U::Clear => defaults.reasoning_effort = None, U::Unchanged => {} }
    let runtime = parse_runtime(&stored.runtime).ok_or_else(|| Refusal::invalid("variant runtime is not known to this cutex"))?;
    validate_defaults(&runtime, &defaults).map_err(|error| Refusal::invalid(format!("defaults: {error:#}")))?;
    stored.defaults = defaults;
    let account = account.clone();
    save(&store)?;
    Ok(account)
}

fn unknown_variant(profile_id: &str, variant_id: &str) -> Refusal {
    Refusal::new(404, "unknown_variant", format!("profile has no variant {variant_id}"),
        json!({ "profile": profile_id, "variantId": variant_id }))
}

pub fn delete_variant(profile: &str, variant_id: &str) -> Result<StoredAccount, Refusal> {
    let (mut store, index) = load_for_update(profile)?;
    let account = &mut store.accounts[index];
    if variant_id == primary_id(account) {
        return Err(Refusal::new(409, "primary_variant_not_removable", "the primary variant is the profile's own runtime",
            json!({ "profile": account.id, "variantId": variant_id })));
    }
    if !account.variants.runtime_variants.iter().any(|variant| variant.id == variant_id) {
        return Err(unknown_variant(&account.id, variant_id));
    }
    let usage = Usage::load();
    let sessions = usage.sessions(&account.id, variant_id);
    if !sessions.is_empty() {
        return Err(Refusal::new(409, "variant_in_use", "agents were created on this variant; it cannot be deleted",
            json!({ "profile": account.id, "variantId": variant_id, "count": sessions.len(),
                    "sessions": sessions.iter().take(20).collect::<Vec<_>>() })));
    }
    account.variants.runtime_variants.retain(|variant| variant.id != variant_id);
    if account.variants.default_variant.as_deref() == Some(variant_id) {
        account.variants.default_variant = None;
    }
    let account = account.clone();
    save(&store)?;
    Ok(account)
}

/// `defaultVariant` of a profile PATCH (`None` = primary). Applied in place.
pub fn set_default_variant(account: &mut StoredAccount, id: Option<&str>) -> anyhow::Result<()> {
    match id {
        None => account.variants.default_variant = None,
        Some(id) => {
            ensure!(variants(account).iter().any(|variant| variant.id == id), "defaultVariant: profile has no variant {id}");
            account.variants.default_variant = (id != primary_id(account)).then(|| id.to_string());
        }
    }
    Ok(())
}

/// `credential` of a DSH profile create / PATCH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialChoice {
    Stored,
    Env(String),
}

/// Apply `apiKey` / `apiKey: null` / `credential` of a DSH profile patch:
/// the key goes to the private key file (never into the account), the
/// source and version to the account. No fallback is configured.
pub fn apply_dsh_credential(account: &mut StoredAccount, patch: &super::settings_api::ProfileApiPatch) -> anyhow::Result<()> {
    ensure!(!(patch.api_key.is_some() && patch.api_key_clear), "apiKey: set or clear, not both");
    match &patch.credential {
        Some(CredentialChoice::Env(name)) => {
            ensure!(patch.api_key.is_none(), "apiKey cannot be stored while choosing an env credential source");
            super::stored_keys::validate_env_source_name(name)?;
            account.options.dsh.api_key_env = name.clone();
            account.variants.credential_source = Some("env".into());
        }
        Some(CredentialChoice::Stored) => account.variants.credential_source = Some("stored".into()),
        None => {}
    }
    if let Some(key) = &patch.api_key {
        let key = super::stored_keys::SecretKey::new(&key.0)?;
        let version = credential_version(account) + 1;
        super::stored_keys::write(&account.id, &key, version)?;
        account.variants.credential_version = version;
        account.variants.credential_source = Some("stored".into());
    }
    if patch.api_key_clear {
        let version = credential_version(account) + 1;
        if super::stored_keys::remove(&account.id, version)? {
            account.variants.credential_version = version;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Launch resolution and binding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundCredential {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// r266: source `kimiAccount`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kimi_account_id: Option<String>,
}

/// Frozen at agent creation; never re-resolved (resume, restart, profile
/// edits). Holds no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileLaunchBinding {
    pub profile_id: String,
    pub variant_id: String,
    pub runtime: String,
    pub credential: BoundCredential,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_package_version: Option<String>,
    pub bound_at: String,
}

impl ProfileLaunchBinding {
    pub fn to_wire(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        for key in ["model", "reasoningEffort", "runtimePackageVersion"] {
            if value.get(key).is_none() {
                value[key] = Value::Null;
            }
        }
        for key in ["envName", "version", "kimiAccountId"] {
            if value["credential"].get(key).is_none() {
                value["credential"][key] = Value::Null;
            }
        }
        value
    }
}

impl ProfileLaunchBinding {
    /// The binding of a fork of the session bound by `self`: the same
    /// profile, variant, runtime and credential source (env name), so the
    /// fork starts exactly like its source. It is a new runtime started
    /// afterwards, so a stored key is the profile's current one (§7: the
    /// fork reads the key at its start) and the bound version says so; a
    /// deleted profile keeps the source's version (the start then fails
    /// `credential_missing`, never another credential). `model` /
    /// `reasoning_effort` / `runtime_package_version` are what the fork
    /// launches with; `boundAt` is now.
    pub fn for_fork(&self, model: Option<String>, reasoning_effort: Option<String>, runtime_package_version: Option<String>) -> Self {
        let mut credential = self.credential.clone();
        if credential.source == Source::Stored.as_str() {
            let current = super::settings_api::load_store().ok().and_then(|store| {
                store.accounts.into_iter().find(|account| account.id == self.profile_id)
            });
            if let Some(account) = current {
                credential.version = Some(credential_version(&account));
            }
        }
        Self {
            profile_id: self.profile_id.clone(),
            variant_id: self.variant_id.clone(),
            runtime: self.runtime.clone(),
            credential,
            model,
            reasoning_effort,
            runtime_package_version,
            bound_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// The resolved launch of a new agent.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub account: StoredAccount,
    pub variant: Variant,
}

impl LaunchPlan {
    pub fn runtime(&self) -> &CliKind {
        &self.variant.runtime
    }

    /// DSH options of this plan: the profile's own options (primary) or the
    /// variant defaults (secondary). A stored key travels as
    /// [`super::stored_keys::DSH_STORED_KEY_ENV`].
    pub fn dsh_options(&self) -> super::dsh::DshOptions {
        let mut options = if self.variant.primary {
            self.account.options.dsh.clone()
        } else {
            super::dsh::DshOptions {
                model: self.variant.defaults.model.clone().unwrap_or_else(|| super::dsh::DEFAULT_MODEL.to_string()),
                reasoning_effort: self.variant.defaults.reasoning_effort.clone(),
                ..Default::default()
            }
        };
        if credential_source(&self.account) == Source::Stored {
            options.api_key_env = super::stored_keys::DSH_STORED_KEY_ENV.to_string();
        }
        options
    }

    /// r266 Kimi: `(model, reasoning_effort)` defaults of this plan (primary:
    /// the Kimi-kind profile's defaults, else its account's default model;
    /// secondary: the variant defaults, else the preset model).
    pub fn kimi_options(&self) -> (Option<String>, Option<String>) {
        let defaults = if self.variant.primary {
            self.account.variants.kimi_defaults.clone().unwrap_or_default()
        } else {
            self.variant.defaults.clone()
        };
        let model = defaults.model.or_else(|| {
            if self.account.cli_kind == CliKind::Kimi {
                super::kimi::profile_catalog(&self.account).default_model
            } else {
                super::kimi::api_key_default_model(&self.account)
            }
        });
        (model, defaults.reasoning_effort)
    }

    pub fn binding(&self, model: Option<String>, reasoning_effort: Option<String>, runtime_package_version: Option<String>) -> ProfileLaunchBinding {
        let source = credential_source(&self.account);
        ProfileLaunchBinding {
            profile_id: self.account.id.clone(),
            variant_id: self.variant.id.clone(),
            runtime: runtime_name(&self.variant.runtime).to_string(),
            credential: BoundCredential {
                source: source.as_str().to_string(),
                env_name: (source == Source::Env).then(|| self.account.options.dsh.api_key_env.clone()),
                version: (source == Source::Stored).then(|| credential_version(&self.account)),
                kimi_account_id: (source == Source::KimiAccount).then(|| self.account.variants.kimi_account.clone()).flatten(),
            },
            model,
            reasoning_effort,
            runtime_package_version,
            bound_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Resolve `variant_id` (None = the profile's default variant) and check the
/// runtime and credential. Refusals carry the Management error code.
pub fn plan(account: &StoredAccount, variant_id: Option<&str>) -> Result<LaunchPlan, Refusal> {
    let all = variants(account);
    let wanted = variant_id.map(str::to_string).unwrap_or_else(|| default_variant_id(account));
    let variant = all.iter().find(|variant| variant.id == wanted).cloned().ok_or_else(|| {
        Refusal::new(400, "unknown_variant", format!("profile {} has no variant {wanted}", account.name),
            json!({ "profile": account.id, "variantId": wanted, "variants": all.iter().map(|variant| &variant.id).collect::<Vec<_>>() }))
    })?;
    // The primary keeps its pre-r265 behaviour (its creators report their own
    // errors); a secondary must be supported on this host now.
    let (status, reason) = compatibility(account, &variant.runtime);
    // r266: Kimi (primary of a Kimi-kind profile too) needs the component the
    // kimi-runtime registers; without it nothing Kimi is created (fail closed).
    if (!variant.primary || variant.runtime == CliKind::Kimi) && status != Status::Supported {
        return Err(Refusal::new(409, "variant_runtime_unavailable", format!("runtime {} is not available for this profile on this host", runtime_name(&variant.runtime)),
            json!({ "profile": account.id, "variantId": variant.id, "runtime": runtime_name(&variant.runtime), "status": status.as_str(), "reason": reason })));
    }
    if variant.runtime == CliKind::Dsh && credential_source(account) == Source::Stored && super::stored_keys::read_for_account(account).is_err() {
        return Err(Refusal::new(409, "credential_missing", format!("profile {} has no stored API key; set one in the profile settings", account.name),
            json!({ "profile": account.id, "variantId": variant.id })));
    }
    if variant.runtime == CliKind::Kimi {
        check_kimi_credential(account, &variant)?;
    }
    // profile-relay-runtimes: DSH / Kimi Code read the profile's interface
    // address (relays included); a relay main address without a DSH address
    // is refused here, before anything is created.
    require_address(account, &variant.runtime, &variant.id)?;
    if credential_source(account) == Source::None {
        return Err(Refusal::new(409, "credential_missing", "the profile's credential source is not known to this cutex", json!({ "profile": account.id })));
    }
    Ok(LaunchPlan { account: account.clone(), variant })
}

fn check_kimi_credential(account: &StoredAccount, variant: &Variant) -> Result<(), Refusal> {
    let details = json!({ "profile": account.id, "variantId": variant.id });
    match credential_source(account) {
        Source::Stored => {
            if super::stored_keys::read_for_account(account).is_err() {
                return Err(Refusal::new(409, "credential_missing", format!("profile {} has no stored API key; set one in the profile settings", account.name), details));
            }
        }
        Source::KimiAccount => {
            let id = account.variants.kimi_account.as_deref().unwrap_or_default();
            let registry = super::kimi_accounts::load().map_err(Refusal::internal)?;
            if registry.find(id).is_none() {
                return Err(Refusal::new(409, "kimi_account_not_found", format!("profile {} names a Kimi account that no longer exists", account.name),
                    json!({ "profile": account.id, "variantId": variant.id, "kimiAccountId": id })));
            }
            match super::kimi_accounts::account_credential_state(id).map_err(Refusal::internal)? {
                super::kimi_accounts::CredentialState::Valid { .. } => {}
                super::kimi_accounts::CredentialState::Revoked => {
                    return Err(Refusal::new(409, "kimi_account_logged_out", "the Kimi account is logged out; log in again",
                        json!({ "profile": account.id, "variantId": variant.id, "kimiAccountId": id })));
                }
                super::kimi_accounts::CredentialState::Missing => {
                    return Err(Refusal::new(409, "credential_missing", "the Kimi account has not logged in yet",
                        json!({ "profile": account.id, "variantId": variant.id, "kimiAccountId": id })));
                }
            }
        }
        Source::None => {
            return Err(Refusal::new(409, "credential_missing", format!("profile {} has no Kimi account; choose one in the profile settings", account.name), details));
        }
        _ => {
            return Err(Refusal::new(409, "variant_runtime_unavailable", "this profile's credential cannot run Kimi", details));
        }
    }
    Ok(())
}

/// Record the binding of a just-created session.
pub fn stamp_binding(session_id: &str, binding: ProfileLaunchBinding) -> anyhow::Result<()> {
    crate::session::store::with_locked_session_rows(&crate::session::store::cutex_sessions_path()?, |rows| {
        let record = rows.session_mut(session_id).ok_or_else(|| anyhow::anyhow!("created session disappeared"))?;
        if record.profile_binding.is_none() {
            record.profile_binding = Some(binding);
        }
        rows.commit().map(drop)
    })
}

/// Paths that cannot launch a secondary variant (agent tools): refuse when
/// the profile's default variant is one, instead of silently using the primary.
pub fn require_primary_default(account: &StoredAccount) -> Result<(), Refusal> {
    let default = default_variant_id(account);
    if default != primary_id(account) {
        return Err(Refusal::new(409, "variant_runtime_unsupported_here",
            format!("profile {}'s default variant {default} cannot be launched from this path; create the agent from the desktop / POST /v2/human/agents", account.name),
            json!({ "profile": account.id, "variantId": default })));
    }
    Ok(())
}

#[cfg(test)]
#[path = "runtime_variants_tests.rs"]
mod tests;
