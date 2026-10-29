//! Provider interface addresses (`profile-relay-runtimes`, generic part;
//! docs/development/profile-relay-runtimes.md).
//!
//! A Profile talks to its provider through one or more interfaces (the
//! OpenAI-compatible API cute-codex uses, DeepSeek's Anthropic-compatible
//! API DSH uses, ...). Every address-bearing interface has the provider's
//! official address as its default and may point at a relay instead (owner
//! decision 2026-10-28: official base URLs are only defaults). Runtime
//! compatibility follows provider + interface, never the URL host; this
//! module only answers "is this address the provider's official one?" so
//! clients never compare hosts themselves.
//!
//! Where an address lives: `openaiCompatible` in the Codex profile's
//! config.toml (`providerBaseUrl`), `anthropicCompatible` in the Claude API
//! account, every other address-bearing interface in the profile's
//! `interface_base_urls` (PATCH `interfaceBaseUrls`). A runtime that does not
//! read its interface's address yet must never be given a relay address it
//! would silently replace by the official host: such writes are refused
//! (`relay_runtime_unavailable`) and the capability stays off until every
//! runtime in [`RELAY_RUNTIME_KINDS`] reads it (this build: all of them).

use serde_json::{json, Value};

use super::model::{CliKind, StoredAccount};

pub const FEATURE: &str = "profile-relay-runtimes";
pub const FEATURE_KEY: &str = "profileRelayRuntimes";

/// DeepSeek's Anthropic-compatible root (DSH `@deepseek-ai/dsh-llm-deepseek`
/// `baseURL` default; `<root>/v1/messages`, `<root>/v1/files`). Not the
/// OpenAI-compatible `https://api.deepseek.com/` cute-codex uses.
pub const DEEPSEEK_ANTHROPIC_BASE_URL: &str = "https://api.deepseek.com/anthropic";
/// Anthropic's own API (a Claude API account without a base URL).
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// Wire refusal code: the profile's main address is a relay and the
/// interface has no address of its own (never guessed, never the official
/// host with a relay key).
pub const BASE_URL_MISSING: &str = "variant_base_url_missing";
/// Wire refusal code: this build's runtime for the interface reads only the
/// official address.
pub const RELAY_RUNTIME_UNAVAILABLE: &str = "relay_runtime_unavailable";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    OpenaiCompatible,
    AnthropicCompatible,
    DeepseekNative,
    KimiNative,
    ChatgptLogin,
    ClaudeSubscription,
    KimiAccount,
}

impl InterfaceKind {
    pub const ALL: [Self; 7] = [
        Self::OpenaiCompatible,
        Self::AnthropicCompatible,
        Self::DeepseekNative,
        Self::KimiNative,
        Self::ChatgptLogin,
        Self::ClaudeSubscription,
        Self::KimiAccount,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCompatible => "openaiCompatible",
            Self::AnthropicCompatible => "anthropicCompatible",
            Self::DeepseekNative => "deepseekNative",
            Self::KimiNative => "kimiNative",
            Self::ChatgptLogin => "chatgptLogin",
            Self::ClaudeSubscription => "claudeSubscription",
            Self::KimiAccount => "kimiAccount",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Subscription logins have no address.
    pub fn has_address(self) -> bool {
        !matches!(self, Self::ChatgptLogin | Self::ClaudeSubscription | Self::KimiAccount)
    }
}

/// Interfaces whose address is stored in `interface_base_urls`.
pub const STORED_KINDS: &[InterfaceKind] = &[InterfaceKind::DeepseekNative];

/// Interfaces whose runtime on this build sends requests to the address the
/// profile holds, relays included. DSH (`deepseekNative`): its launch passes
/// the address as `DEEPSEEK_BASE_URL` (`runtime::dsh::credential`); Kimi Code
/// (`kimiNative`): its config's `providers."cutex-kimi".base_url` is the
/// profile's URL (`profiles::kimi_launch`).
const RELAY_READY: &[InterfaceKind] = &[
    InterfaceKind::OpenaiCompatible,
    InterfaceKind::AnthropicCompatible,
    InterfaceKind::DeepseekNative,
    InterfaceKind::KimiNative,
];

/// The interfaces of this build's ACP runtimes (DSH, Kimi Code):
/// `profile-relay-runtimes` is advertised once all of them are in
/// [`RELAY_READY`].
pub const RELAY_RUNTIME_KINDS: &[InterfaceKind] = &[InterfaceKind::DeepseekNative, InterfaceKind::KimiNative];

pub fn runtime_reads_relay(kind: InterfaceKind) -> bool {
    RELAY_READY.contains(&kind)
}

/// Whether `GET /v2/capabilities` advertises [`FEATURE`].
pub fn relay_runtimes_ready() -> bool {
    ready_with(RELAY_READY, RELAY_RUNTIME_KINDS)
}

fn ready_with(ready: &[InterfaceKind], needed: &[InterfaceKind]) -> bool {
    needed.iter().all(|kind| ready.contains(kind))
}

// ---------------------------------------------------------------------------
// Official addresses
// ---------------------------------------------------------------------------

fn is_provider(provider: &str, id: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(id)
}

/// The provider's official addresses for `kind` (first = default). Empty:
/// no official address (custom, Gemini relays, logins, a provider without
/// that interface).
pub fn official_base_urls(provider: &str, kind: InterfaceKind) -> Vec<&'static str> {
    let preset_urls = || {
        super::providers::API_KEY_PRESETS
            .iter()
            .filter(|preset| is_provider(provider, preset.provider_id))
            .filter_map(|preset| preset.base_url)
            .fold(Vec::new(), |mut urls, url| {
                if !urls.contains(&url) {
                    urls.push(url);
                }
                urls
            })
    };
    match kind {
        InterfaceKind::OpenaiCompatible => preset_urls(),
        // Kimi Code and cute-codex share Kimi's URL (one address per Kimi
        // profile).
        InterfaceKind::KimiNative if super::kimi::is_kimi_provider(&provider.trim().to_ascii_lowercase()) => {
            preset_urls()
        }
        InterfaceKind::DeepseekNative if super::deepseek::is_deepseek_provider(provider) => {
            vec![DEEPSEEK_ANTHROPIC_BASE_URL]
        }
        InterfaceKind::AnthropicCompatible if is_provider(provider, "anthropic") => vec![ANTHROPIC_BASE_URL],
        InterfaceKind::AnthropicCompatible => super::providers::API_KEY_PRESETS
            .iter()
            .filter(|preset| is_provider(provider, preset.provider_id))
            .filter_map(super::providers::ApiKeyPreset::anthropic_base_url)
            .collect(),
        _ => Vec::new(),
    }
}

/// Same endpoint: scheme, host (case-insensitive), port and path without
/// trailing slashes. Unparseable addresses are never the same.
pub fn same_address(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (url::Url::parse(left.trim()), url::Url::parse(right.trim())) else {
        return false;
    };
    left.scheme() == right.scheme()
        && left.host_str().map(|host| host.trim_end_matches('.').to_ascii_lowercase())
            == right.host_str().map(|host| host.trim_end_matches('.').to_ascii_lowercase())
        && left.port_or_known_default() == right.port_or_known_default()
        && left.path().trim_end_matches('/') == right.path().trim_end_matches('/')
        && left.query() == right.query()
}

/// Whether `url` is one of the provider's official addresses for `kind`.
/// Logins are official (no address); a missing address is not.
pub fn is_official(provider: Option<&str>, kind: InterfaceKind, url: Option<&str>) -> bool {
    if !kind.has_address() {
        return true;
    }
    let (Some(provider), Some(url)) = (provider, url) else { return false };
    official_base_urls(provider, kind).iter().any(|official| same_address(official, url))
}

/// The interface's official default: `current` itself when it is official
/// (keeps the region: api.moonshot.cn stays .cn), else the first one.
pub fn default_base_url(provider: Option<&str>, kind: InterfaceKind, current: Option<&str>) -> Option<&'static str> {
    let officials = official_base_urls(provider?, kind);
    current
        .and_then(|current| officials.iter().find(|official| same_address(official, current)))
        .or(officials.first())
        .copied()
}

/// A writable interface address: http(s) with a host, no credentials, no
/// query or fragment. Returns the trimmed text.
pub fn normalize_base_url(text: &str) -> Result<String, String> {
    let text = text.trim();
    let url = url::Url::parse(text).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("must be an http(s) URL with a host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not embed credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not carry a query or fragment".into());
    }
    Ok(text.to_string())
}

// ---------------------------------------------------------------------------
// Per profile
// ---------------------------------------------------------------------------

fn is_api_key_profile(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex
        && account.source.as_deref() == Some("api-key")
        && !account.private_chatgpt_auth_file
}

/// The provider id the official addresses are looked up by: the Codex
/// profile's `model_provider`, `deepseek` for DSH profiles, `anthropic` for
/// Claude API accounts (they record no provider). Never inferred from a URL.
pub fn provider_id(account: &StoredAccount) -> Option<String> {
    match account.cli_kind {
        CliKind::Codex => super::inspect::account_model_provider(account),
        CliKind::Dsh => Some(super::deepseek::DEEPSEEK_PROVIDER_ID.to_string()),
        CliKind::Claude => Some("anthropic".to_string()),
        // r266 Kimi-kind profiles: a Kimi Code membership login (no address).
        CliKind::Kimi => Some(super::providers::KIMI_CODE_PROVIDER_ID.to_string()),
    }
}

/// A Codex API-key profile whose provider id is DeepSeek (any address,
/// relays included): it may carry a DSH variant (B1).
pub fn is_deepseek_api_key(account: &StoredAccount) -> bool {
    account.cli_kind == CliKind::Codex && stored_kinds_for(account).contains(&InterfaceKind::DeepseekNative)
}

/// A Codex API-key profile whose provider id is Kimi / Moonshot (any
/// address, relays included): it may carry a Kimi Code variant (B2).
pub fn is_kimi_api_key(account: &StoredAccount) -> bool {
    is_api_key_profile(account) && provider_id(account).is_some_and(|provider| super::kimi::is_kimi_provider(&provider))
}

/// Interfaces of `account` whose address `interfaceBaseUrls` writes: DSH
/// profiles and DeepSeek API-key profiles (for a DSH variant) hold a
/// `deepseekNative` address.
pub fn stored_kinds_for(account: &StoredAccount) -> Vec<InterfaceKind> {
    let deepseek = match account.cli_kind {
        CliKind::Dsh => true,
        CliKind::Codex if is_api_key_profile(account) => {
            provider_id(account).is_some_and(|provider| super::deepseek::is_deepseek_provider(&provider))
        }
        _ => false,
    };
    if deepseek { vec![InterfaceKind::DeepseekNative] } else { Vec::new() }
}

/// The profile's main address (its OpenAI-compatible base URL), if any.
fn main_address(account: &StoredAccount) -> Option<String> {
    is_api_key_profile(account).then(|| super::inspect::account_model_api_base(account)).flatten()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Official(String),
    Relay(String),
    /// The main address is a relay and this interface has none of its own.
    Missing,
    /// A login: no address.
    None,
}

impl Address {
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Official(url) | Self::Relay(url) => Some(url),
            Self::Missing | Self::None => None,
        }
    }
}

fn classify(provider: Option<&str>, kind: InterfaceKind, url: String) -> Address {
    if is_official(provider, kind, Some(&url)) { Address::Official(url) } else { Address::Relay(url) }
}

/// The address a runtime on `kind` must use for new agents of `account`.
/// A stored address wins; else a `deepseekNative` interface follows the main
/// address: official main (or none: DSH profiles) → DeepSeek's official
/// Anthropic root; relay main → [`Address::Missing`] (never guessed).
pub fn resolve(account: &StoredAccount, kind: InterfaceKind) -> Address {
    if !kind.has_address() {
        return Address::None;
    }
    let provider = provider_id(account);
    let provider = provider.as_deref();
    if let Some(url) = account.interface_base_urls.get(kind.as_str()) {
        return classify(provider, kind, url.clone());
    }
    match kind {
        InterfaceKind::OpenaiCompatible | InterfaceKind::KimiNative => match main_address(account) {
            Some(url) => classify(provider, kind, url),
            None => Address::Missing,
        },
        InterfaceKind::DeepseekNative => match main_address(account) {
            Some(main) if !is_official(provider, InterfaceKind::OpenaiCompatible, Some(&main)) => Address::Missing,
            _ => match default_base_url(provider, kind, None) {
                Some(url) => Address::Official(url.to_string()),
                None => Address::Missing,
            },
        },
        InterfaceKind::AnthropicCompatible => match claude_api_base_url(account) {
            Some(url) => classify(provider, kind, url),
            None => Address::Missing,
        },
        InterfaceKind::ChatgptLogin | InterfaceKind::ClaudeSubscription | InterfaceKind::KimiAccount => Address::None,
    }
}

/// [`resolve`] for a launch: the URL, or the wire refusal code.
pub fn require(account: &StoredAccount, kind: InterfaceKind) -> Result<Option<String>, &'static str> {
    match resolve(account, kind) {
        Address::Official(url) => Ok(Some(url)),
        Address::Relay(url) if runtime_reads_relay(kind) => Ok(Some(url)),
        Address::Relay(_) => Err(RELAY_RUNTIME_UNAVAILABLE),
        Address::Missing => Err(BASE_URL_MISSING),
        Address::None => Ok(None),
    }
}

/// Claude API account address (`None` base URL = Anthropic's own API);
/// `None` for subscription accounts.
fn claude_api_base_url(account: &StoredAccount) -> Option<String> {
    let found = claude_api_account(account)?;
    Some(found.api.and_then(|api| api.base_url).unwrap_or_else(|| ANTHROPIC_BASE_URL.to_string()))
}

fn claude_api_account(account: &StoredAccount) -> Option<crate::runtime::claude_accounts::ClaudeAccount> {
    let id = account.claude_account.as_deref()?;
    crate::runtime::claude_accounts::load_cached()
        .find(id)
        .filter(|found| found.kind == crate::runtime::claude_accounts::ClaudeAccountKind::Api)
        .cloned()
}

/// The interfaces `account` uses, in wire order.
pub fn kinds_for(account: &StoredAccount) -> Vec<InterfaceKind> {
    match account.cli_kind {
        CliKind::Codex if is_api_key_profile(account) => {
            let mut kinds = vec![InterfaceKind::OpenaiCompatible];
            kinds.extend(stored_kinds_for(account));
            if is_kimi_api_key(account) {
                kinds.push(InterfaceKind::KimiNative);
            }
            kinds
        }
        CliKind::Codex => vec![InterfaceKind::ChatgptLogin],
        CliKind::Dsh => vec![InterfaceKind::DeepseekNative],
        CliKind::Kimi => vec![InterfaceKind::KimiAccount],
        CliKind::Claude if claude_api_account(account).is_some() => vec![InterfaceKind::AnthropicCompatible],
        CliKind::Claude => vec![InterfaceKind::ClaudeSubscription],
    }
}

/// One `providerInterfaces[]` row.
pub fn interface_row(provider: Option<&str>, kind: InterfaceKind, address: &Address) -> Value {
    let url = address.url();
    json!({
        "kind": kind.as_str(),
        "baseUrl": url,
        "baseUrlDefault": kind.has_address().then(|| default_base_url(provider, kind, url)).flatten(),
        "official": match address {
            Address::None => true,
            Address::Missing => false,
            _ => is_official(provider, kind, url),
        },
        "relayAccepted": kind.has_address() && runtime_reads_relay(kind),
    })
}

/// `providerInterfaces` of a profile item.
pub fn profile_view(account: &StoredAccount) -> Value {
    let provider = provider_id(account);
    Value::Array(
        kinds_for(account)
            .into_iter()
            .map(|kind| interface_row(provider.as_deref(), kind, &resolve(account, kind)))
            .collect(),
    )
}

/// `providerInterfaces` of a `GET /v2/profile-presets` item: a new profile
/// on the preset's default address.
pub fn preset_view(preset: &super::providers::ApiKeyPreset) -> Value {
    let provider = preset.provider_id;
    let base_url = preset.base_url;
    let address = |kind, url: Option<&str>| match url {
        Some(url) => classify(Some(provider), kind, url.to_string()),
        None => Address::Missing,
    };
    let mut rows = vec![interface_row(Some(provider), InterfaceKind::OpenaiCompatible, &address(InterfaceKind::OpenaiCompatible, base_url))];
    if super::deepseek::is_deepseek_provider(provider) {
        let kind = InterfaceKind::DeepseekNative;
        rows.push(interface_row(Some(provider), kind, &address(kind, default_base_url(Some(provider), kind, None))));
    }
    if super::kimi::is_kimi_provider(provider) {
        let kind = InterfaceKind::KimiNative;
        rows.push(interface_row(Some(provider), kind, &address(kind, base_url)));
    }
    if let Some(url) = preset.anthropic_base_url() {
        let kind = InterfaceKind::AnthropicCompatible;
        rows.push(interface_row(Some(provider), kind, &address(kind, Some(url))));
    }
    Value::Array(rows)
}

/// Apply `interfaceBaseUrls` (`Some` = set, `None` = back to the default).
pub fn apply_patch(
    account: &mut StoredAccount,
    patch: &std::collections::BTreeMap<String, Option<String>>,
) -> anyhow::Result<()> {
    if patch.is_empty() {
        return Ok(());
    }
    let allowed = stored_kinds_for(account);
    let provider = provider_id(account);
    for (key, url) in patch {
        let kind = InterfaceKind::parse(key)
            .filter(|kind| allowed.contains(kind))
            .ok_or_else(|| anyhow::anyhow!("interfaceBaseUrls.{key}: this profile has no such stored interface address"))?;
        match url {
            None => {
                account.interface_base_urls.remove(key);
            }
            Some(url) => {
                let url = normalize_base_url(url).map_err(|message| anyhow::anyhow!("interfaceBaseUrls.{key}: {message}"))?;
                anyhow::ensure!(
                    runtime_reads_relay(kind) || is_official(provider.as_deref(), kind, Some(&url)),
                    "interfaceBaseUrls.{key}: {RELAY_RUNTIME_UNAVAILABLE}: the runtime for this interface reads only the \
                     provider's official address on this host"
                );
                account.interface_base_urls.insert(key.clone(), url);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "interface_urls_tests.rs"]
mod tests;
