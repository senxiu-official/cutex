//! Profile provider category v2 (`profile.appearance.providerCategory`,
//! capability `profile-provider-category-v2`).
//!
//! Categories: kimi, glm (bigmodel.cn), zai (Z.ai international), claude,
//! deepseek, codex (formerly `chatgpt`), other.
//!
//! Storage keeps older binaries working (docs/usage-pricing.md §Rollback):
//! the legacy `options.provider` (`chatgpt|deepseek|glm|claude|other`, read
//! by r216 and earlier with a strict enum inside a `deny_unknown_fields`
//! struct) is always written with the nearest legacy value, and the exact
//! v2 value goes into the account-level `provider_category` field, which old
//! binaries ignore (and drop when they save the profile store).
//! The v2 value counts only while its legacy projection still equals
//! `options.provider`; otherwise an older binary (or the TUI legacy picker)
//! changed the category since and the legacy value wins.

use super::appearance::Provider;
use super::model::{CliKind, StoredAccount};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProviderCategory {
    Kimi,
    Glm,
    Zai,
    Claude,
    Deepseek,
    Codex,
    Other,
}

impl ProviderCategory {
    pub const ALL: [Self; 7] = [
        Self::Kimi,
        Self::Glm,
        Self::Zai,
        Self::Claude,
        Self::Deepseek,
        Self::Codex,
        Self::Other,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Kimi => "kimi",
            Self::Glm => "glm",
            Self::Zai => "zai",
            Self::Claude => "claude",
            Self::Deepseek => "deepseek",
            Self::Codex => "codex",
            Self::Other => "other",
        }
    }

    /// Strict v2 keys plus the legacy `chatgpt` (= codex).
    pub fn parse(value: &str) -> Option<Self> {
        if value == "chatgpt" {
            return Some(Self::Codex);
        }
        Self::ALL.into_iter().find(|category| category.key() == value)
    }

    /// Nearest value of the legacy (r216) enum.
    pub fn legacy(self) -> Provider {
        match self {
            Self::Codex => Provider::Chatgpt,
            Self::Glm | Self::Zai => Provider::Glm,
            Self::Deepseek => Provider::Deepseek,
            Self::Claude => Provider::Claude,
            Self::Kimi | Self::Other => Provider::Other,
        }
    }

    pub fn from_legacy(provider: Provider) -> Self {
        match provider {
            Provider::Chatgpt => Self::Codex,
            Provider::Deepseek => Self::Deepseek,
            Provider::Glm => Self::Glm,
            Provider::Claude => Self::Claude,
            Provider::Other => Self::Other,
        }
    }
}

/// The owner's explicit category, `None` when unset.
pub fn manual(account: &StoredAccount) -> Option<ProviderCategory> {
    manual_from(account.options.provider, account.provider_category.as_deref())
}

fn manual_from(legacy: Option<Provider>, stored: Option<&str>) -> Option<ProviderCategory> {
    let legacy = legacy?;
    match stored.and_then(ProviderCategory::parse) {
        Some(category) if category.legacy() == legacy => Some(category),
        _ => Some(ProviderCategory::from_legacy(legacy)),
    }
}

/// Set (or clear with `None`) the explicit category in both storage fields.
pub fn set_manual(account: &mut StoredAccount, category: Option<ProviderCategory>) {
    account.options.provider = category.map(ProviderCategory::legacy);
    account.provider_category = category.map(|category| category.key().to_string());
}

/// Category inferred from backend facts when none is set: ChatGPT login →
/// codex; Claude runtime → claude; then the API base URL host; then the
/// configured `model_provider` name; else other.
pub fn infer(
    cli_kind: &CliKind,
    auth_kind: &str,
    base_url: Option<&str>,
    model_provider: Option<&str>,
) -> ProviderCategory {
    if auth_kind == "chatgpt" {
        return ProviderCategory::Codex;
    }
    if *cli_kind == CliKind::Claude {
        return ProviderCategory::Claude;
    }
    if let Some(category) = base_url.and_then(category_for_url) {
        return category;
    }
    model_provider
        .and_then(category_for_provider_name)
        .unwrap_or(ProviderCategory::Other)
}

/// Category of an API base URL by host (exact domain or a subdomain).
pub fn category_for_url(url: &str) -> Option<ProviderCategory> {
    let host = url::Url::parse(url.trim()).ok()?.host_str()?.to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    let matches = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    const HOSTS: &[(&str, ProviderCategory)] = &[
        ("bigmodel.cn", ProviderCategory::Glm),
        ("z.ai", ProviderCategory::Zai),
        ("moonshot.cn", ProviderCategory::Kimi),
        ("moonshot.ai", ProviderCategory::Kimi),
        ("kimi.com", ProviderCategory::Kimi),
        ("kimi.ai", ProviderCategory::Kimi),
        ("deepseek.com", ProviderCategory::Deepseek),
        ("anthropic.com", ProviderCategory::Claude),
        ("openai.com", ProviderCategory::Codex),
        ("chatgpt.com", ProviderCategory::Codex),
    ];
    HOSTS
        .iter()
        .find(|(domain, _)| matches(domain))
        .map(|(_, category)| *category)
}

/// Category hinted by a `model_provider` / usage attribution provider name.
pub fn category_for_provider_name(name: &str) -> Option<ProviderCategory> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "glm" | "zhipu" | "zhipuai" | "bigmodel" => ProviderCategory::Glm,
        "zai" | "z.ai" | "z-ai" => ProviderCategory::Zai,
        "kimi" | "moonshot" | "moonshotai" | "kimi-code" => ProviderCategory::Kimi,
        "deepseek" => ProviderCategory::Deepseek,
        "claude" | "anthropic" => ProviderCategory::Claude,
        "openai" | "chatgpt" | "codex" => ProviderCategory::Codex,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_and_legacy_chatgpt_reads_as_codex() {
        for category in ProviderCategory::ALL {
            assert_eq!(ProviderCategory::parse(category.key()), Some(category));
        }
        assert_eq!(ProviderCategory::parse("chatgpt"), Some(ProviderCategory::Codex));
        assert_eq!(ProviderCategory::parse("ChatGPT"), None);
        assert_eq!(ProviderCategory::parse("openai"), None);
    }

    #[test]
    fn manual_value_survives_only_while_the_legacy_projection_matches() {
        assert_eq!(manual_from(None, Some("kimi")), None);
        assert_eq!(manual_from(Some(Provider::Other), Some("kimi")), Some(ProviderCategory::Kimi));
        assert_eq!(manual_from(Some(Provider::Glm), Some("zai")), Some(ProviderCategory::Zai));
        // An older binary changed options.provider (and kept our field only
        // because it did not save yet): the legacy value wins.
        assert_eq!(manual_from(Some(Provider::Deepseek), Some("kimi")), Some(ProviderCategory::Deepseek));
        assert_eq!(manual_from(Some(Provider::Chatgpt), None), Some(ProviderCategory::Codex));
        assert_eq!(manual_from(Some(Provider::Glm), Some("future")), Some(ProviderCategory::Glm));
        // Every category round-trips through both storage fields.
        for category in ProviderCategory::ALL {
            assert_eq!(manual_from(Some(category.legacy()), Some(category.key())), Some(category));
        }
    }

    #[test]
    fn legacy_values_stay_readable_by_r216() {
        // r216 deserializes options.provider with serde snake_case keys.
        for category in ProviderCategory::ALL {
            let json = serde_json::to_string(&category.legacy()).unwrap();
            assert!(
                ["\"chatgpt\"", "\"deepseek\"", "\"glm\"", "\"claude\"", "\"other\""].contains(&json.as_str()),
                "{json}"
            );
        }
    }

    #[test]
    fn inference_order_and_hosts() {
        use ProviderCategory::*;
        assert_eq!(infer(&CliKind::Codex, "chatgpt", Some("https://open.bigmodel.cn/api"), None), Codex);
        assert_eq!(infer(&CliKind::Claude, "claude", None, None), Claude);
        assert_eq!(infer(&CliKind::Claude, "apiKey", Some("https://api.moonshot.cn/anthropic"), None), Claude);
        let url = |u: &str| infer(&CliKind::Codex, "apiKey", Some(u), None);
        assert_eq!(url("https://open.bigmodel.cn/api/coding/paas/v4"), Glm);
        assert_eq!(url("https://api.z.ai/api/paas/v4"), Zai);
        assert_eq!(url("https://api.moonshot.cn/v1"), Kimi);
        assert_eq!(url("https://api.moonshot.ai/v1"), Kimi);
        assert_eq!(url("https://api.kimi.com/coding/v1"), Kimi);
        assert_eq!(url("https://api.kimi.ai/coding/v1"), Kimi);
        assert_eq!(url("https://api.deepseek.com"), Deepseek);
        assert_eq!(url("https://api.anthropic.com/v1"), Claude);
        assert_eq!(url("https://api.openai.com/v1"), Codex);
        assert_eq!(url("https://example.com/v1"), Other);
        // Look-alike hosts do not match.
        assert_eq!(url("https://notz.ai/v1"), Other);
        assert_eq!(url("https://bigmodel.cn.evil.example/v1"), Other);
        assert_eq!(infer(&CliKind::Codex, "apiKey", None, Some("DeepSeek")), Deepseek);
        assert_eq!(infer(&CliKind::Codex, "apiKey", Some("https://proxy.example/v1"), Some("zai")), Zai);
        assert_eq!(infer(&CliKind::Codex, "none", None, None), Other);
    }
}
