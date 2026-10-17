//! Profile-owned HTTP quota reader. UI consumers receive only normalized display data.
use super::model::{AccountsStore, StoredAccount};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub url: String,
    pub auth: Auth,
    pub label: String,
    pub refresh_seconds: u64,
    pub mode: Mode,
    pub value_pointer: String,
    pub limit_pointer: String,
    pub reset_pointer: String,
    pub reset_format: ResetFormat,
    /// Display-only window caption (e.g. "5h", "week"), at most 8 characters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_label: Option<String>,
    /// A window the provider may leave out of its response (e.g. the Kimi
    /// Code weekly window, gone on the new plans): an optional gauge that
    /// has never read a value is not reported instead of showing an empty
    /// stale gauge. Absent = false (older binaries keep it in `extra`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// Fields a newer cutex wrote (kept on save; persistence is not
    /// `deny_unknown_fields`, RFC backend-auto-update §6.2).
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// `Authorization: Bearer <profile API key>`.
    ProfileApiKey,
    /// `Authorization: <profile API key>` without the `Bearer` prefix (Zhipu
    /// Coding Plan quota API).
    ProfileApiKeyRaw,
    None,
}
/// Stored `quota_preset` (wire values unchanged for older binaries).
/// `Auto` is legacy: the host-keyed automatic reader was removed
/// (`profile-generic-switches`); the one-time migration
/// (`profiles::generic_switches`) turned every resolvable `auto` into an
/// explicit [`Reader`]. An `auto` left over reports a preset error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Custom,
    Auto,
}

/// A named quota reader the owner picks for a profile (`StoredAccount::
/// quota_reader`). No reader is chosen by host or provider name: the owner
/// (or the API-key preset the profile was created from) names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reader {
    /// `GET <profile base URL>/usage`, OpenAI-style `rate_limits[]`
    /// (`remaining` / `limit` / `reset_at` RFC 3339). Generic: any provider
    /// or relay that serves this shape.
    UsageEndpoint,
    /// DeepSeek official balance API.
    DeepseekBalance,
    /// Kimi Open Platform (global, api.moonshot.ai) balance, USD.
    MoonshotBalance,
    /// Kimi Open Platform China (api.moonshot.cn) balance, CNY.
    MoonshotCnBalance,
    /// Zhipu / Z.ai GLM Coding Plan quota on the profile base URL's origin
    /// (5 h window; a profile created from the preset adds the week).
    ZhipuCodingPlan,
    /// OpenRouter key credits.
    OpenrouterKey,
    /// Kimi Code membership plan (`GET <base URL>/usages` on the Kimi Code
    /// API, api.kimi.com / api.kimi.ai `/coding/v1`): 5 h rate window; a
    /// profile created from the preset adds the weekly (legacy plans) and
    /// monthly total windows.
    KimiCodePlan,
}

impl Reader {
    pub const ALL: [Reader; 7] = [
        Reader::UsageEndpoint,
        Reader::DeepseekBalance,
        Reader::MoonshotBalance,
        Reader::MoonshotCnBalance,
        Reader::ZhipuCodingPlan,
        Reader::OpenrouterKey,
        Reader::KimiCodePlan,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Reader::UsageEndpoint => "usage_endpoint",
            Reader::DeepseekBalance => "deepseek_balance",
            Reader::MoonshotBalance => "moonshot_balance",
            Reader::MoonshotCnBalance => "moonshot_cn_balance",
            Reader::ZhipuCodingPlan => "zhipu_coding_plan",
            Reader::OpenrouterKey => "openrouter_key",
            Reader::KimiCodePlan => "kimi_code_plan",
        }
    }

    pub fn parse(value: &str) -> Option<Reader> {
        Reader::ALL.into_iter().find(|reader| reader.as_str() == value)
    }

    /// Whether the reader derives its URL from the profile's base URL.
    pub fn needs_base_url(self) -> bool {
        matches!(self, Reader::UsageEndpoint | Reader::ZhipuCodingPlan | Reader::KimiCodePlan)
    }
}

/// The quota choice of a profile as Management clients see it
/// (`/v2/profiles/{profile}/quota-config` `preset`): `custom`, a [`Reader`]
/// name, or the read-only legacy `auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Custom,
    LegacyAuto,
    Reader(Reader),
}

impl Choice {
    pub fn of(account: &StoredAccount) -> Choice {
        Choice::from_parts(account.quota_preset, account.quota_reader)
    }

    /// The choice of the stored `quota_preset` / `quota_reader` pair.
    pub fn from_parts(preset: Preset, reader: Option<Reader>) -> Choice {
        match (reader, preset) {
            (Some(reader), _) => Choice::Reader(reader),
            (None, Preset::Auto) => Choice::LegacyAuto,
            (None, Preset::Custom) => Choice::Custom,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Choice::Custom => "custom",
            Choice::LegacyAuto => "auto",
            Choice::Reader(reader) => reader.as_str(),
        }
    }

    /// A choice a client may set: `custom` or a reader name. `auto` is
    /// refused (the host-keyed automatic reader was removed).
    pub fn parse_settable(value: &str) -> Result<Choice, String> {
        match value {
            "custom" => Ok(Choice::Custom),
            "auto" => Err(format!(
                "preset auto was removed (profile-generic-switches); choose custom or a reader: {}",
                Reader::ALL.map(Reader::as_str).join(", ")
            )),
            other => Reader::parse(other).map(Choice::Reader).ok_or_else(|| {
                format!(
                    "unknown quota preset {other:?}; use custom or a reader: {}",
                    Reader::ALL.map(Reader::as_str).join(", ")
                )
            }),
        }
    }

    /// Every value `parse_settable` accepts.
    pub fn settable_values() -> Vec<&'static str> {
        std::iter::once("custom").chain(Reader::ALL.map(Reader::as_str)).collect()
    }

    /// Store the choice on `account`: the legacy field stays `custom`.
    pub fn apply(self, account: &mut StoredAccount) {
        match self {
            Choice::Custom => {
                account.quota_preset = Preset::Custom;
                account.quota_reader = None;
            }
            Choice::LegacyAuto => {
                account.quota_preset = Preset::Auto;
                account.quota_reader = None;
            }
            Choice::Reader(reader) => {
                account.quota_preset = Preset::Custom;
                account.quota_reader = Some(reader);
            }
        }
    }
}
impl Default for Preset {
    fn default() -> Self {
        Self::Custom
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    RemainingAndLimit,
    UsedAndLimit,
    RemainingPercent,
    UsedPercent,
    /// Used share as a fraction 0..1 (e.g. Kimi Code `used_ratio`).
    UsedRatio,
    Balance,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetFormat {
    Rfc3339,
    UnixSeconds,
    UnixMillis,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            auth: Auth::ProfileApiKey,
            label: "Q".into(),
            refresh_seconds: 60,
            mode: Mode::RemainingAndLimit,
            value_pointer: "/rate_limits/0/remaining".into(),
            limit_pointer: "/rate_limits/0/limit".into(),
            reset_pointer: "/rate_limits/0/reset_at".into(),
            reset_format: ResetFormat::Rfc3339,
            window_label: None,
            optional: false,
            extra: Default::default(),
        }
    }
}
impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            (10..=86400).contains(&self.refresh_seconds),
            "Refresh interval must be 10–86400 seconds"
        );
        ensure!(
            !self.label.is_empty()
                && self.label.len() <= 6
                && self.label.bytes().all(|c| c.is_ascii_alphanumeric()),
            "Quota label must be 1–6 ASCII letters/digits"
        );
        if let Some(window) = &self.window_label {
            ensure!(
                !window.trim().is_empty()
                    && window.chars().count() <= 8
                    && !window.chars().any(char::is_control),
                "Window label must be 1–8 characters"
            );
        }
        for p in [
            &self.value_pointer,
            &self.limit_pointer,
            &self.reset_pointer,
        ] {
            ensure!(
                p.len() <= 512
                    && (p.is_empty() || p.starts_with('/'))
                    && !p.chars().any(char::is_control),
                "Use a JSON Pointer, e.g. /rate_limits/0/remaining"
            );
            let mut chars = p.chars();
            while let Some(c) = chars.next() {
                if c == '~' {
                    ensure!(
                        matches!(chars.next(), Some('0' | '1')),
                        "JSON Pointer escapes must be ~0 or ~1"
                    );
                }
            }
        }
        if self.enabled || !self.url.is_empty() {
            let u = url::Url::parse(&self.url).context("Invalid quota URL")?;
            ensure!(
                matches!(u.scheme(), "http" | "https")
                    && u.host_str().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.fragment().is_none(),
                "Quota URL must be HTTP(S), without embedded credentials or fragment"
            );
        }
        Ok(())
    }

    /// The effective gauge config for a quota choice: `Custom` validates the
    /// stored config, a [`Reader`] fills endpoint, auth, mode and pointers
    /// (label / window caption / refresh / `enabled` stay the stored ones;
    /// the default label `Q` becomes the reader's), legacy `auto` fails.
    pub fn resolve(&self, choice: Choice, base_url: Option<&str>) -> anyhow::Result<Config> {
        let reader = match choice {
            Choice::Custom => {
                let next = self.clone();
                next.validate()?;
                return Ok(next);
            }
            Choice::LegacyAuto => anyhow::bail!(
                "The automatic quota preset was removed; choose custom or a reader ({})",
                Reader::ALL.map(Reader::as_str).join(", ")
            ),
            Choice::Reader(reader) => reader,
        };
        let base_url = base_url
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let mut next = self.clone();
        next.auth = Auth::ProfileApiKey;
        let base = || {
            base_url.with_context(|| {
                format!("Quota reader {} needs the profile's base URL", reader.as_str())
            })
        };
        match reader {
            Reader::ZhipuCodingPlan => {
                let url = url::Url::parse(base()?).context("Invalid base URL")?;
                let origin = url.origin().ascii_serialization();
                next.url = format!("{origin}{ZHIPU_QUOTA_PATH}");
                next.auth = Auth::ProfileApiKeyRaw;
                next.mode = Mode::UsedPercent;
                next.value_pointer = format!("{ZHIPU_FIVE_HOUR}/percentage");
                next.limit_pointer = String::new();
                next.reset_pointer = format!("{ZHIPU_FIVE_HOUR}/nextResetTime");
                next.reset_format = ResetFormat::UnixMillis;
                if next.label == "Q" {
                    next.label = "GLM".into();
                }
                if next.window_label.is_none() {
                    next.window_label = Some("5h".into());
                }
            }
            Reader::KimiCodePlan => {
                next.url = kimi_code_usages_url(base()?)?;
                next.mode = Mode::UsedAndLimit;
                next.value_pointer = format!("{KIMI_FIVE_HOUR}/used");
                next.limit_pointer = format!("{KIMI_FIVE_HOUR}/limit");
                next.reset_pointer = format!("{KIMI_FIVE_HOUR}/resetTime");
                next.reset_format = ResetFormat::Rfc3339;
                if next.label == "Q" {
                    next.label = "Kimi".into();
                }
                if next.window_label.is_none() {
                    next.window_label = Some("5h".into());
                }
            }
            Reader::UsageEndpoint => {
                next.url = format!("{}/usage", base()?.trim_end_matches('/'));
                next.mode = Mode::RemainingAndLimit;
                next.value_pointer = "/rate_limits/0/remaining".into();
                next.limit_pointer = "/rate_limits/0/limit".into();
                next.reset_pointer = "/rate_limits/0/reset_at".into();
                next.reset_format = ResetFormat::Rfc3339;
            }
            Reader::DeepseekBalance
            | Reader::MoonshotBalance
            | Reader::MoonshotCnBalance
            | Reader::OpenrouterKey => {
                let balance = match reader {
                    Reader::DeepseekBalance => DEEPSEEK_BALANCE,
                    Reader::MoonshotBalance => MOONSHOT_GLOBAL_BALANCE,
                    Reader::MoonshotCnBalance => MOONSHOT_CN_BALANCE,
                    _ => OPENROUTER_KEY,
                };
                next.url = balance.url.into();
                next.mode = Mode::Balance;
                next.value_pointer = balance.value_pointer.into();
                next.limit_pointer = String::new();
                next.reset_pointer = String::new();
                if next.label == "Q" {
                    next.label = balance.label.into();
                }
            }
        }
        next.validate()?;
        Ok(next)
    }

    /// Extra gauges a new profile with `reader` starts with (same response
    /// as the primary window; an empty URL inherits the primary endpoint and
    /// auth): the Zhipu Coding Plan weekly window; the Kimi Code weekly
    /// window (legacy plans only, optional) and monthly total (optional).
    pub fn extras_for_new_profile(reader: Reader) -> Vec<Config> {
        if reader == Reader::KimiCodePlan {
            return vec![
                Config {
                    enabled: true,
                    url: String::new(),
                    label: "Kimi".into(),
                    mode: Mode::UsedAndLimit,
                    value_pointer: format!("{KIMI_WEEK}/used"),
                    limit_pointer: format!("{KIMI_WEEK}/limit"),
                    reset_pointer: format!("{KIMI_WEEK}/resetTime"),
                    reset_format: ResetFormat::Rfc3339,
                    window_label: Some("week".into()),
                    optional: true,
                    ..Config::default()
                },
                Config {
                    enabled: true,
                    url: String::new(),
                    label: "Kimi".into(),
                    mode: Mode::UsedRatio,
                    value_pointer: format!("{KIMI_MONTH}/used_ratio"),
                    limit_pointer: String::new(),
                    reset_pointer: format!("{KIMI_MONTH}/reset_time"),
                    reset_format: ResetFormat::Rfc3339,
                    window_label: Some("month".into()),
                    optional: true,
                    ..Config::default()
                },
            ];
        }
        if reader != Reader::ZhipuCodingPlan {
            return Vec::new();
        }
        vec![Config {
            enabled: true,
            url: String::new(),
            label: "GLM".into(),
            mode: Mode::UsedPercent,
            value_pointer: format!("{ZHIPU_WEEK}/percentage"),
            limit_pointer: String::new(),
            reset_pointer: format!("{ZHIPU_WEEK}/nextResetTime"),
            reset_format: ResetFormat::UnixMillis,
            window_label: Some("week".into()),
            ..Config::default()
        }]
    }

    pub fn resolve_for_account(&self, account: &StoredAccount) -> anyhow::Result<Config> {
        let base_url = match Choice::of(account) {
            Choice::Reader(reader) if reader.needs_base_url() => {
                super::inspect::account_model_api_base(account)
            }
            _ => None,
        };
        self.resolve(Choice::of(account), base_url.as_deref())
    }
}

/// Set `choice` on `account` and, when it resolves, store the resolved
/// endpoint in `options.quota` too: a binary without readers (r235 and
/// older ignore `quota_reader`, see `docs/PROFILE-GENERIC-SWITCHES.md`)
/// then reads the same gauge as a custom config.
pub fn apply_choice(account: &mut StoredAccount, choice: Choice) -> anyhow::Result<()> {
    choice.apply(account);
    if let Choice::Reader(_) = choice {
        let resolved = account.options.quota.resolve_for_account(account)?;
        account.options.quota = resolved;
    }
    Ok(())
}

/// A new API-key profile created from a preset that names a quota reader
/// starts with it on (plus its extra gauges). Only on the preset's own base
/// URL (or none given): a different base URL is the owner's choice and
/// keeps the quota off.
pub fn enable_preset_reader(
    account: &mut StoredAccount,
    reader: Option<Reader>,
    preset_base_url: Option<&str>,
    base_url: Option<&str>,
) {
    let Some(reader) = reader else { return };
    let same_base = |a: &str, b: &str| a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/');
    let on_preset_url = match (base_url, preset_base_url) {
        (None, _) => true,
        (Some(given), Some(preset)) => same_base(given, preset),
        (Some(_), None) => false,
    };
    if !on_preset_url {
        return;
    }
    account.quota_preset = Preset::Custom;
    account.quota_reader = Some(reader);
    account.options.quota.enabled = true;
    // Resolved endpoint stored too (see `apply_choice`).
    if let Ok(resolved) = account
        .options
        .quota
        .resolve(Choice::Reader(reader), base_url.or(preset_base_url))
    {
        account.options.quota = resolved;
    }
    account.options.quotas = Config::extras_for_new_profile(reader);
}

/// Zhipu / Z.ai GLM Coding Plan quota API: `GET <origin>/api/monitor/usage/quota/limit`
/// with `authorization: <API key>` (no `Bearer`). `data.limits[]` entries are
/// matched by `type` / `unit` / `number`: 5 h window = `TOKENS_LIMIT` unit 3
/// number 5, week = `TOKENS_LIMIT` unit 6, MCP month = `TIME_LIMIT` unit 5
/// number 1 (2026-10: Z.ai answers `CREDIT_LIMIT` instead of `TOKENS_LIMIT`
/// for the same windows, so the token windows match by unit / number only);
/// `percentage` is the USED share, `nextResetTime` unix ms
/// (optional). Source: Zhipu's own `zai-org/ZCode` repository
/// (`docs/coding-plan-quota-remaining-api.md`,
/// `docs/bigmodel-coding-plan-usage-quota.md`), not the public API reference.
const ZHIPU_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const ZHIPU_FIVE_HOUR: &str = "/data/limits/[unit=3,number=5]";
const ZHIPU_WEEK: &str = "/data/limits/[unit=6]";

/// Kimi Code membership usage (`GET https://api.kimi.com/coding/v1/usages`,
/// overseas `api.kimi.ai`; `Authorization: Bearer <Kimi Code API key>`).
/// Not in Kimi's public docs; the shape is the one the official Kimi CLI
/// reads for `/usage` (MoonshotAI/kimi-cli `src/kimi_cli/ui/shell/usage.py`)
/// and CodexBar's Kimi provider fixtures (steipete/CodexBar
/// `KimiModels.swift`, tests from captured responses):
///
/// - `limits[]`: `{window: {duration: 300, timeUnit: "TIME_UNIT_MINUTE"},
///   detail: {limit, used, remaining, resetTime}}`, counts as strings or
///   numbers: the rolling 5 h rate window (every plan);
/// - `usage`: `{limit, used, remaining, resetTime}`: the weekly quota
///   (legacy plans only; new plans have no weekly window);
/// - `usages`: ratio pools `{used_ratio: 0..1, reset_time}` keyed
///   `limit_5h`, `limit_7d`, `limit_month_total` (the membership's monthly
///   total shared with Kimi web), `limit_month_code`.
///
/// Reset times are RFC 3339 with up to nanoseconds.
const KIMI_FIVE_HOUR: &str = "/limits/0/detail";
const KIMI_WEEK: &str = "/usage";
const KIMI_MONTH: &str = "/usages/limit_month_total";

/// `<base>/usages` for a Kimi Code API base URL: `…/coding/v1` as given,
/// `…/coding` (the Anthropic-compatible base) gains `/v1`, any other path
/// is replaced by `/coding/v1` on the same origin.
fn kimi_code_usages_url(base: &str) -> anyhow::Result<String> {
    let url = url::Url::parse(base.trim()).context("Invalid base URL")?;
    let path = url.path().trim_end_matches('/');
    let origin = url.origin().ascii_serialization();
    Ok(if path.ends_with("/coding/v1") {
        format!("{origin}{path}/usages")
    } else if path.ends_with("/coding") {
        format!("{origin}{path}/v1/usages")
    } else {
        format!("{origin}/coding/v1/usages")
    })
}

/// Pointers stored by earlier releases (the weekly extra in `options.quotas`)
/// still select `type=TOKENS_LIMIT`; on the Zhipu endpoint they match by
/// unit / number like the current constants.
fn zhipu_pointer_compat(pointer: &str) -> String {
    pointer.replace("[type=TOKENS_LIMIT,", "[")
}

/// A provider-documented balance endpoint, read with the profile's API key as
/// a Bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BalanceReader {
    url: &'static str,
    value_pointer: &'static str,
    label: &'static str,
    /// Currency of the balance when the response does not name one.
    currency: Option<&'static str>,
}

/// DeepSeek `GET /user/balance` (https://api-docs.deepseek.com/api/get-user-balance):
/// `balance_infos[].total_balance` with `balance_infos[].currency` CNY | USD.
const DEEPSEEK_BALANCE: BalanceReader = BalanceReader {
    url: "https://api.deepseek.com/user/balance",
    value_pointer: "/balance_infos/0/total_balance",
    label: "DS",
    currency: None,
};
/// Kimi Open Platform global (platform.kimi.ai, https://platform.kimi.ai/docs/api/balance):
/// `GET https://api.moonshot.ai/v1/users/me/balance`, `data.available_balance`
/// (cash + voucher) in USD.
const MOONSHOT_GLOBAL_BALANCE: BalanceReader = BalanceReader {
    url: "https://api.moonshot.ai/v1/users/me/balance",
    value_pointer: "/data/available_balance",
    label: "Kimi",
    currency: Some("USD"),
};
/// Kimi Open Platform China (platform.kimi.com, https://platform.kimi.com/docs/api/balance):
/// same API on api.moonshot.cn, balances in CNY (人民币元). Keys of the two
/// platforms are separate, so each host reads its own endpoint.
const MOONSHOT_CN_BALANCE: BalanceReader = BalanceReader {
    url: "https://api.moonshot.cn/v1/users/me/balance",
    value_pointer: "/data/available_balance",
    label: "Kimi",
    currency: Some("CNY"),
};
/// OpenRouter `GET /api/v1/key` (https://openrouter.ai/docs/api/reference/limits):
/// `data.limit_remaining` credits (USD), `null` when the key has no limit.
const OPENROUTER_KEY: BalanceReader = BalanceReader {
    url: "https://openrouter.ai/api/v1/key",
    value_pointer: "/data/limit_remaining",
    label: "OR",
    currency: Some("USD"),
};
const BALANCE_READERS: &[BalanceReader] =
    &[DEEPSEEK_BALANCE, MOONSHOT_GLOBAL_BALANCE, MOONSHOT_CN_BALANCE, OPENROUTER_KEY];

/// Fixed currency of a known balance endpoint whose response has none.
fn documented_balance_currency(url: &str) -> Option<&'static str> {
    BALANCE_READERS
        .iter()
        .find(|reader| reader.url == url)
        .and_then(|reader| reader.currency)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Display {
    pub label: String,
    pub remaining_percent: Option<f64>,
    #[serde(default)]
    pub balance: Option<String>,
    pub reset_at: Option<i64>,
    pub observed_at: Option<i64>,
    pub stale: bool,
}
/// JSON Pointer lookup with one extension: a segment `[key=value,…]` selects
/// the first element of an array whose fields all equal the given values
/// (compared as strings, so `unit=3` matches `3` and `"3"`). Plain pointers
/// behave exactly like `Value::pointer`.
pub(crate) fn lookup<'a>(v: &'a Value, p: &str) -> Option<&'a Value> {
    if !p.contains('[') {
        return v.pointer(p);
    }
    if p.is_empty() {
        return Some(v);
    }
    let mut current = v;
    for raw in p.strip_prefix('/')?.split('/') {
        if let Some(selector) = raw.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            let conditions: Vec<(&str, &str)> = selector
                .split(',')
                .map(|condition| condition.split_once('='))
                .collect::<Option<_>>()?;
            current = current.as_array()?.iter().find(|item| {
                conditions.iter().all(|(key, expected)| match item.get(key.trim()) {
                    Some(Value::String(actual)) => actual == expected.trim(),
                    Some(Value::Number(actual)) => actual.to_string() == expected.trim(),
                    Some(Value::Bool(actual)) => actual.to_string() == expected.trim(),
                    _ => false,
                })
            })?;
            continue;
        }
        let token = raw.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&token)?,
            Value::Array(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn number(v: &Value, p: &str) -> anyhow::Result<f64> {
    let v = lookup(v, p).context("Quota numeric field missing")?;
    let n = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .context("Quota field is not numeric")?;
    ensure!(n.is_finite(), "Quota field is not finite");
    Ok(n)
}
pub fn parse(c: &Config, v: &Value, now: i64) -> anyhow::Result<Display> {
    c.validate()?;
    // OpenRouter: a key without a credit limit reports `limit_remaining: null`.
    if c.mode == Mode::Balance
        && c.url == OPENROUTER_KEY.url
        && lookup(v, &c.value_pointer).is_some_and(Value::is_null)
    {
        return Ok(Display {
            label: c.label.clone(),
            remaining_percent: None,
            balance: Some("no limit".into()),
            reset_at: None,
            observed_at: Some(now),
            stale: false,
        });
    }
    let n = number(v, &c.value_pointer)?;
    ensure!(n >= 0.0, "Quota value is negative");
    if c.mode == Mode::Balance {
        let currency = v
            .pointer("/balance_infos/0/currency")
            .and_then(Value::as_str)
            .or_else(|| documented_balance_currency(&c.url))
            .unwrap_or_default();
        let balance = if currency.eq_ignore_ascii_case("cny") {
            format!("¥{n:.2}")
        } else if currency.is_empty() {
            format!("{n:.2}")
        } else {
            format!("{currency} {n:.2}")
        };
        return Ok(Display {
            label: c.label.clone(),
            remaining_percent: None,
            balance: Some(balance),
            reset_at: None,
            observed_at: Some(now),
            stale: false,
        });
    }
    let percent = match c.mode {
        Mode::RemainingAndLimit | Mode::UsedAndLimit => {
            let total = number(v, &c.limit_pointer)?;
            ensure!(total > 0.0, "Quota limit must be positive");
            if c.mode == Mode::RemainingAndLimit {
                100.0 * n / total
            } else {
                100.0 * (1.0 - n / total)
            }
        }
        Mode::RemainingPercent => n,
        Mode::UsedPercent => 100.0 - n,
        Mode::UsedRatio => 100.0 * (1.0 - n),
        Mode::Balance => anyhow::bail!("Balance mode has no percentage"),
    };
    ensure!(percent.is_finite(), "Quota percentage is not finite");
    // A missing / null reset field (optional in some APIs, e.g. Zhipu's
    // `nextResetTime`) shows the gauge without a reset time.
    let reset_raw = (!c.reset_pointer.is_empty())
        .then(|| lookup(v, &c.reset_pointer))
        .flatten()
        .filter(|raw| !raw.is_null());
    let reset_at = match reset_raw {
        None => None,
        Some(raw) => Some(match c.reset_format {
            ResetFormat::Rfc3339 => chrono::DateTime::parse_from_rfc3339(
                raw.as_str().context("Reset must be RFC3339")?,
            )?
            .timestamp(),
            ResetFormat::UnixSeconds | ResetFormat::UnixMillis => {
                let n = raw
                    .as_i64()
                    .or_else(|| raw.as_str().and_then(|s| s.parse().ok()))
                    .context("Reset must be an integer timestamp")?;
                if c.reset_format == ResetFormat::UnixMillis {
                    n / 1000
                } else {
                    n
                }
            }
        })
    };
    Ok(Display {
        label: c.label.clone(),
        remaining_percent: Some(percent.clamp(0.0, 100.0)),
        balance: None,
        reset_at,
        observed_at: Some(now),
        stale: false,
    })
}
#[derive(Serialize, Deserialize)]
struct Cache {
    fingerprint: String,
    next_read: i64,
    display: Display,
}
fn accounts() -> anyhow::Result<AccountsStore> {
    Ok(serde_json::from_slice(&std::fs::read(
        crate::config::paths::accounts_path()?,
    )?)?)
}
fn auth_path(a: &StoredAccount) -> anyhow::Result<PathBuf> {
    Ok(super::materialize::materialized_profiles_dir()?
        .join(&a.id)
        .join("auth.json"))
}
fn fetch(c: &Config, auth: &[u8], now: i64) -> anyhow::Result<Display> {
    parse(c, &fetch_json(c, auth)?, now)
}
fn fetch_json(c: &Config, auth: &[u8]) -> anyhow::Result<Value> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(8)))
        .max_redirects(0)
        .build()
        .new_agent();
    let mut req = agent.get(&c.url).header("Accept", "application/json");
    if matches!(c.auth, Auth::ProfileApiKey | Auth::ProfileApiKeyRaw) {
        let v: Value = serde_json::from_slice(auth).context("Profile credential unavailable")?;
        let key = v["OPENAI_API_KEY"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Profile has no API key")?;
        let value = if c.auth == Auth::ProfileApiKey { format!("Bearer {key}") } else { key.to_string() };
        req = req.header("Authorization", &value);
    }
    let mut response = req.call().map_err(|e| match e {
        ureq::Error::StatusCode(s) => anyhow::anyhow!("Quota endpoint returned HTTP {s}"),
        _ => anyhow::anyhow!("Quota endpoint unavailable"),
    })?;
    ensure!(
        response.status().is_success(),
        "Quota endpoint did not return success"
    );
    let v: Value = response
        .body_mut()
        .with_config()
        .limit(512 * 1024)
        .read_json()
        .map_err(|_| anyhow::anyhow!("Invalid or oversized quota JSON"))?;
    Ok(v)
}
/// Shared per-profile cache and nonblocking lock prevent multiple TUI windows from multiplying requests.
pub fn read(a: &StoredAccount, force: bool) -> anyhow::Result<Option<Display>> {
    read_cached(
        a,
        force,
        &crate::config::paths::runtime_dir()?.join("profile-quota"),
        fetch,
    )
}

/// Refresh a named profile through the shared cache without changing the active profile.
pub fn read_named(profile: &str) -> anyhow::Result<Option<Display>> {
    let store = accounts()?;
    let account = store
        .accounts
        .iter()
        .find(|account| account.id == profile || account.name == profile)
        .context("Profile not found")?;
    read(account, false)
}

/// Read the last successful private cache mapping without network I/O.
pub fn cached(id: &str, c: &Config, auth: &[u8]) -> Option<Display> {
    cached_in(
        id,
        c,
        auth,
        &crate::config::paths::runtime_dir()
            .ok()?
            .join("profile-quota"),
    )
}

fn cached_in(id: &str, c: &Config, auth: &[u8], dir: &std::path::Path) -> Option<Display> {
    if !c.enabled {
        return None;
    }
    c.validate().ok()?;
    let fingerprint = cache_fingerprint(c, auth).ok()?;
    let id = uuid::Uuid::parse_str(id).ok()?;
    let path = dir.join(format!("{id}.json"));
    let cached = serde_json::from_slice::<Cache>(&std::fs::read(path).ok()?).ok()?;
    (cached.fingerprint == fingerprint).then_some(cached.display)
}

fn cache_fingerprint(c: &Config, auth: &[u8]) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(c)?);
    hasher.update(auth);
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_cached(
    a: &StoredAccount,
    force: bool,
    dir: &std::path::Path,
    fetch: impl FnOnce(&Config, &[u8], i64) -> anyhow::Result<Display>,
) -> anyhow::Result<Option<Display>> {
    if !a.options.quota.enabled {
        return Ok(None);
    }
    let resolved = a.options.quota.resolve_for_account(a)?;
    read_resolved_cached(a, &resolved, None, force, dir, fetch).map(Some)
}

/// Cached read of one resolved gauge. The primary gauge keeps the historic
/// `<profile-id>.json` cache file; extras use `<profile-id>-x<fp16>.json`
/// keyed by their config fingerprint so editing one extra never invalidates
/// another.
fn read_resolved_cached(
    a: &StoredAccount,
    c: &Config,
    extra: Option<()>,
    force: bool,
    dir: &std::path::Path,
    fetch: impl FnOnce(&Config, &[u8], i64) -> anyhow::Result<Display>,
) -> anyhow::Result<Display> {
    c.validate()?;
    let auth = if matches!(c.auth, Auth::ProfileApiKey | Auth::ProfileApiKeyRaw) {
        std::fs::read(auth_path(a)?).unwrap_or_default()
    } else {
        Vec::new()
    };
    let fingerprint = cache_fingerprint(c, &auth)?;
    std::fs::create_dir_all(dir)?;
    let id = uuid::Uuid::parse_str(&a.id).context("Invalid profile ID")?;
    let stem = match extra {
        None => id.to_string(),
        Some(()) => format!("{id}-x{}", &fingerprint[..16]),
    };
    let path = dir.join(format!("{stem}.json"));
    let mut cached = std::fs::read(&path)
        .ok()
        .and_then(|v| serde_json::from_slice::<Cache>(&v).ok())
        .filter(|v| v.fingerprint == fingerprint);
    let now = chrono::Utc::now().timestamp();
    let placeholder = || Display {
        label: c.label.clone(),
        remaining_percent: None,
        balance: None,
        reset_at: None,
        observed_at: None,
        stale: true,
    };
    if !force && cached.as_ref().is_some_and(|v| v.next_read > now) {
        return Ok(cached.map(|v| v.display).unwrap_or_else(placeholder));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(format!("{stem}.lock")))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(cached.map(|v| v.display).unwrap_or_else(placeholder));
    }
    // Another reader may have refreshed after our first read and before lock acquisition.
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(v) = serde_json::from_slice::<Cache>(&bytes) {
            if v.fingerprint == fingerprint {
                if !force && v.next_read > now {
                    return Ok(v.display);
                }
                cached = Some(v);
            }
        }
    }
    let result = fetch(c, &auth, now);
    let display = match result {
        Ok(d) => d,
        Err(_) => {
            let mut d = cached.map(|v| v.display).unwrap_or_else(placeholder);
            d.stale = true;
            d
        }
    };
    let next = Cache {
        fingerprint,
        next_read: now + c.refresh_seconds as i64,
        display: display.clone(),
    };
    crate::config::atomic::write_private_pretty_json_atomic(&path, &next, "profile quota cache")?;
    Ok(display)
}
/// Maximum number of extra gauges (`options.quotas`) per profile.
pub const MAX_EXTRA_QUOTAS: usize = 8;

/// One normalized gauge for Management clients. `id` is `primary` for
/// `options.quota` and `extra-<n>` (0-based index into `options.quotas`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gauge {
    pub id: String,
    pub label: String,
    /// `window` (percentage with optional reset) or `balance`.
    pub kind: &'static str,
    pub remaining_percent: Option<f64>,
    pub balance: Option<String>,
    pub reset_at: Option<i64>,
    pub window_label: Option<String>,
    pub observed_at: Option<i64>,
    pub stale: bool,
    /// Configuration problem that prevented any read (e.g. no auto preset).
    pub error: Option<String>,
    /// Used share of the window (native rate-limit gauges only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// `native` for gauges normalized from the native `account/rateLimits`
    /// data; absent for quota-config (HTTP reader) gauges.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
}

fn gauge_kind(c: &Config) -> &'static str {
    if c.mode == Mode::Balance {
        "balance"
    } else {
        "window"
    }
}

/// Resolve extras. An extra with an empty `url` inherits the resolved
/// primary endpoint and auth (e.g. the GLM weekly window from the same
/// `/usage` response with different pointers).
pub fn resolve_extras(a: &StoredAccount) -> Vec<(usize, anyhow::Result<Config>)> {
    let primary = a.options.quota.resolve_for_account(a).ok();
    a.options
        .quotas
        .iter()
        .enumerate()
        .filter(|(_, c)| c.enabled)
        .map(|(index, c)| {
            let mut next = c.clone();
            if next.url.is_empty() {
                match &primary {
                    Some(primary) if !primary.url.is_empty() => {
                        next.url = primary.url.clone();
                        next.auth = primary.auth;
                    }
                    _ => {
                        return (
                            index,
                            Err(anyhow::anyhow!(
                                "Extra quota has no URL and the primary endpoint is unavailable"
                            )),
                        )
                    }
                }
            }
            if next.url.ends_with(ZHIPU_QUOTA_PATH) {
                next.value_pointer = zhipu_pointer_compat(&next.value_pointer);
                next.limit_pointer = zhipu_pointer_compat(&next.limit_pointer);
                next.reset_pointer = zhipu_pointer_compat(&next.reset_pointer);
            }
            let checked = next.validate().map(|_| next);
            (index, checked)
        })
        .collect()
}

/// Read every enabled gauge of a profile through the shared private cache.
/// One HTTP response is reused for gauges that share a URL within a call.
pub fn gauges(a: &StoredAccount, force: bool) -> anyhow::Result<Vec<Gauge>> {
    let gauges = gauges_in(
        a,
        force,
        &crate::config::paths::runtime_dir()?.join("profile-quota"),
        fetch_json,
    )?;
    // Best-effort trend snapshots (throttled per gauge); never fails the read.
    crate::management::v2::usage::record_quota_gauges(&a.id, &a.name, &gauges);
    Ok(gauges)
}

fn gauges_in(
    a: &StoredAccount,
    force: bool,
    dir: &std::path::Path,
    fetch_json: impl Fn(&Config, &[u8]) -> anyhow::Result<Value>,
) -> anyhow::Result<Vec<Gauge>> {
    let memo: std::cell::RefCell<std::collections::HashMap<String, Result<Value, String>>> =
        Default::default();
    let fetch = |c: &Config, auth: &[u8], now: i64| -> anyhow::Result<Display> {
        let key = format!("{:?}\n{}", c.auth, c.url);
        let value = memo
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| fetch_json(c, auth).map_err(|error| error.to_string()))
            .clone()
            .map_err(anyhow::Error::msg)?;
        parse(c, &value, now)
    };
    let gauge = |id: String, c: &Config, read: anyhow::Result<Display>| match read {
        Ok(d) => Gauge {
            id,
            label: d.label,
            kind: gauge_kind(c),
            remaining_percent: d.remaining_percent,
            balance: d.balance,
            reset_at: d.reset_at,
            window_label: c.window_label.clone(),
            observed_at: d.observed_at,
            stale: d.stale,
            error: None,
            used_percent: None,
            source: None,
        },
        Err(error) => Gauge {
            id,
            label: c.label.clone(),
            kind: gauge_kind(c),
            remaining_percent: None,
            balance: None,
            reset_at: None,
            window_label: c.window_label.clone(),
            observed_at: None,
            stale: true,
            error: Some(format!("{error:#}")),
            used_percent: None,
            source: None,
        },
    };
    let mut out = Vec::new();
    if a.options.quota.enabled {
        let primary = &a.options.quota;
        let read = primary
            .resolve_for_account(a)
            .and_then(|resolved| {
                read_resolved_cached(a, &resolved, None, force, dir, &fetch)
                    .map(|display| (resolved, display))
            });
        match read {
            Ok((resolved, display)) => out.push(gauge("primary".into(), &resolved, Ok(display))),
            Err(error) => out.push(gauge("primary".into(), primary, Err(error))),
        }
    }
    for (index, resolved) in resolve_extras(a) {
        let id = format!("extra-{index}");
        match resolved {
            Ok(c) => {
                let read = read_resolved_cached(a, &c, Some(()), force, dir, &fetch);
                // An optional window that has never been read (the response
                // does not carry it) is left out; one read before keeps its
                // last value as a stale gauge like any other.
                let never_read = |d: &Display| d.observed_at.is_none() && d.remaining_percent.is_none() && d.balance.is_none();
                if c.optional && read.as_ref().is_ok_and(never_read) {
                    continue;
                }
                out.push(gauge(id, &c, read));
            }
            Err(error) => out.push(gauge(id, &a.options.quotas[index], Err(error))),
        }
    }
    Ok(out)
}

/// Find a profile by exact ID or name.
pub fn find_profile(profile: &str) -> anyhow::Result<Option<StoredAccount>> {
    Ok(accounts()?
        .accounts
        .into_iter()
        .find(|account| account.id == profile || account.name == profile))
}

/// Secret-free quota configuration view for Management clients.
pub fn config_view(a: &StoredAccount) -> Value {
    let (resolved, preset_error) = match a.options.quota.resolve_for_account(a) {
        Ok(resolved) => (Some(resolved), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    serde_json::json!({
        "contractVersion": 2,
        "profile": { "id": a.id, "name": a.name },
        // `custom`, a reader name (`quota::Reader`) or the legacy `auto`
        // (read-only; resolves to presetError).
        "preset": Choice::of(a).as_str(),
        // Values PUT accepts for `preset` (profile-generic-switches).
        "presetOptions": Choice::settable_values(),
        "primary": a.options.quota,
        "extras": a.options.quotas,
        "resolvedPrimary": resolved,
        "presetError": preset_error,
    })
}

/// Replace a profile's quota preset and gauge configs atomically through the
/// profile store. Every config is validated before anything is written.
pub fn update_config(
    profile: &str,
    preset: Choice,
    primary: Config,
    extras: Vec<Config>,
) -> anyhow::Result<StoredAccount> {
    ensure!(
        extras.len() <= MAX_EXTRA_QUOTAS,
        "At most {MAX_EXTRA_QUOTAS} extra quota gauges are allowed"
    );
    let mut store = accounts()?;
    let account = store
        .accounts
        .iter_mut()
        .find(|a| a.id == profile || a.name == profile)
        .context("Profile not found")?;
    let mut candidate = account.clone();
    preset.apply(&mut candidate);
    candidate.options.quota = primary;
    candidate.options.quotas = extras;
    // Primary: an enabled reader is checked through its resolution (the
    // stored URL may legitimately be empty); otherwise validate as stored.
    if candidate.options.quota.enabled {
        candidate
            .options
            .quota
            .resolve_for_account(&candidate)
            .context("primary is invalid")?;
    } else {
        candidate.options.quota.validate().context("primary is invalid")?;
    }
    // Extras: an empty URL means "inherit the primary endpoint".
    for (index, extra) in candidate.options.quotas.iter().enumerate() {
        let mut shape = extra.clone();
        if shape.url.is_empty() {
            shape.url = "https://inherit.invalid/".into();
        }
        shape
            .validate()
            .with_context(|| format!("extras[{index}] is invalid"))?;
    }
    for (index, resolved) in resolve_extras(&candidate) {
        resolved.with_context(|| format!("extras[{index}] is invalid"))?;
    }
    // A reader also stores its resolved endpoint in the primary config, so a
    // binary without readers reads the same gauge (best effort: a reader
    // that does not resolve yet, e.g. no base URL, keeps the stored config).
    if let Choice::Reader(_) = preset {
        if let Ok(resolved) = candidate.options.quota.resolve_for_account(&candidate) {
            candidate.options.quota = resolved;
        }
    }
    account.quota_preset = candidate.quota_preset;
    account.quota_reader = candidate.quota_reader;
    account.options.quota = candidate.options.quota;
    account.options.quotas = candidate.options.quotas;
    let updated = account.clone();
    super::store::save_store(&store)?;
    Ok(updated)
}

pub fn for_thread(thread: &str) -> anyhow::Result<Option<Display>> {
    let store = accounts()?;
    let selected = std::env::var("CUTEX_SESSION_PROFILE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            crate::launch::session_display::read(thread)
                .ok()
                .flatten()
                .map(|p| p.profile_id)
        })
        .or_else(|| {
            crate::session::store::load_cutex_session_store()
                .ok()
                .and_then(|s| {
                    s.sessions
                        .values()
                        .find(|r| r.app_server_thread_id() == Some(thread))
                        .and_then(|r| r.profile.clone())
                })
        });
    let Some(selected) = selected else {
        return Ok(None);
    };
    let a = super::lookup::find_account(&store, &selected)?;
    a.map(|a| read(a, false)).transpose().map(Option::flatten)
}
pub fn command(
    profile: &str,
    file: Option<&std::path::Path>,
    disable: bool,
    refresh: bool,
) -> anyhow::Result<Value> {
    let mut store = accounts()?;
    let a = store
        .accounts
        .iter_mut()
        .find(|a| a.id == profile || a.name == profile)
        .context("Profile not found")?;
    if let Some(file) = file {
        let c: Config = serde_json::from_slice(&std::fs::read(file)?)?;
        c.validate()?;
        a.options.quota = c;
    }
    if disable {
        a.options.quota.enabled = false;
    }
    let a = a.clone();
    if file.is_some() || disable {
        super::store::save_store(&store)?;
    }
    let display = read(&a, refresh)?;
    Ok(serde_json::json!({"profile":a.name,"config":a.options.quota,"display":display}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_reuses_success_retains_stale_data_and_invalidates_mapping() {
        let dir = std::env::temp_dir().join(format!("cutex-quota-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut a: StoredAccount = serde_json::from_value(
            serde_json::json!({"id":uuid::Uuid::new_v4().to_string(),"name":"fixture"}),
        )
        .unwrap();
        a.options.quota = Config {
            enabled: true,
            url: "https://example.com/usage".into(),
            auth: Auth::None,
            ..Config::default()
        };
        let first = read_cached(&a, false, &dir, |c, _, now| {
            Ok(Display {
                label: c.label.clone(),
                remaining_percent: Some(75.0),
                balance: None,
                reset_at: Some(now + 3600),
                observed_at: Some(now),
                stale: false,
            })
        })
        .unwrap();
        let second = read_cached(&a, false, &dir, |_, _, _| {
            panic!("cache should avoid another request")
        })
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            cached_in(&a.id, &a.options.quota, &[], &dir),
            first,
            "cache peek must reuse the persisted mapping without a fetch"
        );
        let stale = read_cached(&a, true, &dir, |_, _, _| anyhow::bail!("offline"))
            .unwrap()
            .unwrap();
        assert_eq!(stale.remaining_percent, Some(75.0));
        assert!(stale.stale);
        a.options.quota.value_pointer = "/different".into();
        let invalidated = read_cached(&a, false, &dir, |_, _, _| anyhow::bail!("missing field"))
            .unwrap()
            .unwrap();
        assert_eq!(invalidated.remaining_percent, None);
        assert!(invalidated.stale);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn usage_endpoint_units_and_timezone_are_not_balance_or_percent() {
        let c = Config::default();
        let d=parse(&c,&serde_json::json!({"rate_limits":[{"limit":150,"used":36.4798552,"remaining":113.5201448,"reset_at":"2026-09-26T00:00:00+08:00"}]}),1).unwrap();
        assert!((d.remaining_percent.unwrap() - 75.6800965333).abs() < 0.00001);
        assert_eq!(
            d.reset_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-09-25T16:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
    }

    #[test]
    fn readers_resolve_usage_endpoint_and_balances_without_host_rules() {
        let mut config = Config::default();
        config.enabled = true;
        config.url = String::new();
        let resolve = |reader: Reader, base: Option<&str>| config.resolve(Choice::Reader(reader), base);

        // Generic usage endpoint: any base URL, no domain or provider rule.
        for base in ["https://relay.example/v1", "https://relay.example/v1/", "http://127.0.0.1:9/api"] {
            let usage = resolve(Reader::UsageEndpoint, Some(base)).unwrap();
            assert_eq!(usage.url, format!("{}/usage", base.trim_end_matches('/')));
            assert_eq!(usage.mode, Mode::RemainingAndLimit);
            assert_eq!(usage.value_pointer, "/rate_limits/0/remaining");
            assert_eq!(usage.label, "Q", "no provider-name label");
        }
        let err = resolve(Reader::UsageEndpoint, None).unwrap_err();
        assert!(err.to_string().contains("needs the profile's base URL"), "{err}");
        assert!(resolve(Reader::UsageEndpoint, Some("  ")).is_err());

        // Official balance readers: fixed endpoints, whatever the base URL.
        for (reader, url, label) in [
            (Reader::DeepseekBalance, "https://api.deepseek.com/user/balance", "DS"),
            (Reader::MoonshotBalance, "https://api.moonshot.ai/v1/users/me/balance", "Kimi"),
            (Reader::MoonshotCnBalance, "https://api.moonshot.cn/v1/users/me/balance", "Kimi"),
            (Reader::OpenrouterKey, "https://openrouter.ai/api/v1/key", "OR"),
        ] {
            for base in [None, Some("https://relay.example/v1")] {
                let balance = resolve(reader, base).unwrap();
                assert_eq!(balance.url, url, "{reader:?}");
                assert_eq!(balance.mode, Mode::Balance);
                assert_eq!(balance.auth, Auth::ProfileApiKey);
                assert_eq!((balance.limit_pointer.as_str(), balance.reset_pointer.as_str()), ("", ""));
                assert_eq!(balance.label, label);
            }
        }
        // A custom label survives.
        let labelled = Config { label: "Mine".into(), ..Config::default() };
        assert_eq!(labelled.resolve(Choice::Reader(Reader::DeepseekBalance), None).unwrap().label, "Mine");

        // Legacy auto never resolves any more.
        let error = config.resolve(Choice::LegacyAuto, Some("https://api.deepseek.com")).unwrap_err();
        assert!(error.to_string().contains("automatic quota preset was removed"), "{error}");
        // Custom is untouched.
        let custom = Config { url: "https://example.com/q".into(), ..Config::default() };
        assert_eq!(custom.resolve(Choice::Custom, Some("https://api.z.ai/v4")).unwrap(), custom);
    }

    #[test]
    fn quota_choice_strings_and_storage() {
        assert_eq!(Choice::parse_settable("custom"), Ok(Choice::Custom));
        assert_eq!(Choice::parse_settable("usage_endpoint"), Ok(Choice::Reader(Reader::UsageEndpoint)));
        assert!(Choice::parse_settable("auto").unwrap_err().contains("removed"));
        assert!(Choice::parse_settable("relay").is_err());
        for reader in Reader::ALL {
            assert_eq!(Reader::parse(reader.as_str()), Some(reader));
            assert_eq!(serde_json::to_value(reader).unwrap(), reader.as_str());
        }
        assert_eq!(Choice::settable_values().len(), Reader::ALL.len() + 1);
        let mut account = StoredAccount::from_import(
            "alpha".into(),
            &crate::profiles::model::ImportedSnapshot {
                raw_auth_json: "{}".into(),
                raw_config_toml: None,
                raw_model_catalog_json: None,
                email: None,
                plan_type: None,
                source: "api-key".into(),
            },
            Default::default(),
        );
        Choice::Reader(Reader::DeepseekBalance).apply(&mut account);
        assert_eq!((account.quota_preset, account.quota_reader), (Preset::Custom, Some(Reader::DeepseekBalance)));
        assert_eq!(Choice::of(&account), Choice::Reader(Reader::DeepseekBalance));
        let stored = serde_json::to_value(&account).unwrap();
        assert_eq!(stored["quota_preset"], "custom", "older binaries read custom");
        assert_eq!(stored["quota_reader"], "deepseek_balance");
        apply_choice(&mut account, Choice::Reader(Reader::DeepseekBalance)).unwrap();
        assert_eq!(account.options.quota.url, "https://api.deepseek.com/user/balance", "resolved endpoint stored");
        Choice::Custom.apply(&mut account);
        assert!(serde_json::to_value(&account).unwrap().get("quota_reader").is_none());
        account.quota_preset = Preset::Auto;
        assert_eq!(Choice::of(&account), Choice::LegacyAuto);
    }

    #[test]
    fn preset_reader_only_on_the_preset_base_url() {
        let new = || StoredAccount::from_import(
            "alpha".into(),
            &crate::profiles::model::ImportedSnapshot {
                raw_auth_json: "{}".into(),
                raw_config_toml: None,
                raw_model_catalog_json: None,
                email: None,
                plan_type: None,
                source: "api-key".into(),
            },
            Default::default(),
        );
        let preset = Some("https://api.z.ai/api/v1");
        for base in [None, Some("https://api.z.ai/api/v1/")] {
            let mut account = new();
            enable_preset_reader(&mut account, Some(Reader::ZhipuCodingPlan), preset, base);
            assert_eq!(account.quota_reader, Some(Reader::ZhipuCodingPlan));
            assert!(account.options.quota.enabled);
            assert_eq!(account.options.quotas.len(), 1, "weekly window");
        }
        let mut proxied = new();
        enable_preset_reader(&mut proxied, Some(Reader::ZhipuCodingPlan), preset, Some("https://relay.example/v1"));
        assert_eq!(proxied.quota_reader, None);
        assert!(!proxied.options.quota.enabled);
        let mut none = new();
        enable_preset_reader(&mut none, None, preset, None);
        assert_eq!(none.quota_reader, None);
    }

    /// 2026-10 Z.ai answers `CREDIT_LIMIT` windows (plan `level`); the 5 h
    /// gauge and a weekly extra stored by an earlier release
    /// (`type=TOKENS_LIMIT` pointers) both read it.
    #[test]
    fn zhipu_credit_limit_windows_read_with_current_and_stored_pointers() {
        let body = serde_json::json!({"code": 200, "success": true, "data": {"level": "pro", "limits": [
            {"type": "CREDIT_LIMIT", "unit": 3, "number": 5, "usage": 1000, "currentValue": 10, "remaining": 990, "percentage": 1, "nextResetTime": 1_792_026_017_798_i64},
            {"type": "CREDIT_LIMIT", "unit": 6, "number": 1, "usage": 5000, "currentValue": 2000, "remaining": 3000, "percentage": 40, "nextResetTime": 1_792_328_921_997_i64}
        ]}});
        let config = Config { enabled: true, ..Config::default() };
        let primary = config.resolve(Choice::Reader(Reader::ZhipuCodingPlan), Some("https://api.z.ai/api/v1")).unwrap();
        let shown = parse(&primary, &body, 1).unwrap();
        assert_eq!(shown.remaining_percent, Some(99.0));
        assert_eq!(shown.reset_at, Some(1_792_026_017));
        let mut stored_week = Config::extras_for_new_profile(Reader::ZhipuCodingPlan).remove(0);
        stored_week.value_pointer = "/data/limits/[type=TOKENS_LIMIT,unit=6]/percentage".into();
        stored_week.reset_pointer = "/data/limits/[type=TOKENS_LIMIT,unit=6]/nextResetTime".into();
        stored_week.url = primary.url.clone();
        stored_week.auth = primary.auth;
        stored_week.value_pointer = zhipu_pointer_compat(&stored_week.value_pointer);
        stored_week.reset_pointer = zhipu_pointer_compat(&stored_week.reset_pointer);
        let week = parse(&stored_week, &body, 1).unwrap();
        assert_eq!(week.remaining_percent, Some(60.0));
        assert_eq!(zhipu_pointer_compat("/data/limits/[type=TIME_LIMIT,unit=5,number=1]/currentValue"), "/data/limits/[type=TIME_LIMIT,unit=5,number=1]/currentValue");
    }

    /// The documented response of Zhipu's Coding Plan quota API (ZCode
    /// repository docs): 5 h, week and MCP-month limits.
    fn zhipu_quota_sample(with_reset: bool) -> serde_json::Value {
        let mut five_hour = serde_json::json!({"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 37});
        if with_reset {
            five_hour["nextResetTime"] = serde_json::json!(1_760_400_000_000_i64);
        }
        serde_json::json!({"code": 200, "success": true, "data": {"level": "pro", "limits": [
            {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 4, "usage": 1000, "currentValue": 40},
            {"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 12, "nextResetTime": 1_760_900_000_000_i64},
            five_hour
        ]}})
    }

    #[test]
    fn zhipu_coding_plan_reader_picks_limits_by_type_unit_and_number() {
        let config = Config::default();
        for (base, origin) in [
            ("https://api.z.ai/api/v1", "https://api.z.ai"),
            ("https://api.z.ai/api/coding/paas/v4", "https://api.z.ai"),
            ("https://open.bigmodel.cn/api/v1", "https://open.bigmodel.cn"),
            ("https://bigmodel.cn/api/v1", "https://bigmodel.cn"),
        ] {
            let resolved = config.resolve(Choice::Reader(Reader::ZhipuCodingPlan), Some(base)).unwrap();
            assert_eq!(resolved.url, format!("{origin}/api/monitor/usage/quota/limit"), "{base}");
            assert_eq!(resolved.auth, Auth::ProfileApiKeyRaw);
            assert_eq!(resolved.label, "GLM");
            assert_eq!(resolved.window_label.as_deref(), Some("5h"));
        }
        let primary = config.resolve(Choice::Reader(Reader::ZhipuCodingPlan), Some("https://api.z.ai/api/v1")).unwrap();
        let display = parse(&primary, &zhipu_quota_sample(true), 9).unwrap();
        assert_eq!(display.remaining_percent, Some(63.0));
        assert_eq!(display.reset_at, Some(1_760_400_000));
        // nextResetTime is optional.
        let display = parse(&primary, &zhipu_quota_sample(false), 9).unwrap();
        assert_eq!((display.remaining_percent, display.reset_at), (Some(63.0), None));

        // The weekly extra inherits the primary endpoint and auth.
        let extras = Config::extras_for_new_profile(Reader::ZhipuCodingPlan);
        assert_eq!(extras.len(), 1);
        let mut week = extras[0].clone();
        week.url = primary.url.clone();
        week.auth = primary.auth;
        let display = parse(&week, &zhipu_quota_sample(true), 9).unwrap();
        assert_eq!((display.remaining_percent, display.reset_at), (Some(88.0), Some(1_760_900_000)));
        assert_eq!(week.window_label.as_deref(), Some("week"));
        assert!(Config::extras_for_new_profile(Reader::DeepseekBalance).is_empty());
        assert!(Config::extras_for_new_profile(Reader::UsageEndpoint).is_empty());
    }

    #[test]
    fn raw_auth_sends_the_key_without_bearer() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(request.starts_with("get /api/monitor/usage/quota/limit "), "{request}");
            assert!(request.contains("authorization: fixture-key\r\n"), "no Bearer prefix");
            let body = zhipu_quota_sample(false).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let config = Config {
            enabled: true,
            url: format!("http://{addr}/api/monitor/usage/quota/limit"),
            auth: Auth::ProfileApiKeyRaw,
            mode: Mode::UsedPercent,
            value_pointer: format!("{ZHIPU_FIVE_HOUR}/percentage"),
            limit_pointer: String::new(),
            reset_pointer: format!("{ZHIPU_FIVE_HOUR}/nextResetTime"),
            reset_format: ResetFormat::UnixMillis,
            ..Config::default()
        };
        let display = fetch(&config, br#"{"OPENAI_API_KEY":"fixture-key"}"#, 5).unwrap();
        server.join().unwrap();
        assert_eq!(display.remaining_percent, Some(63.0));
    }

    #[test]
    fn selector_pointers_extend_json_pointer_without_changing_plain_lookups() {
        let body = zhipu_quota_sample(true);
        assert_eq!(lookup(&body, "/data/level"), body.pointer("/data/level"));
        assert_eq!(lookup(&body, "/data/limits/0/unit"), Some(&serde_json::json!(5)));
        assert_eq!(lookup(&body, "/data/limits/[type=TIME_LIMIT,unit=5,number=1]/currentValue"), Some(&serde_json::json!(40)));
        assert_eq!(lookup(&body, "/data/limits/[type=TOKENS_LIMIT,unit=9]/percentage"), None);
        assert_eq!(lookup(&body, "/data/limits/[broken]/percentage"), None);
        assert_eq!(lookup(&serde_json::json!({"a/b": {"[x]": 1}}), "/a~1b/[x]"), None);
        assert_eq!(lookup(&serde_json::json!({"a/b": 2}), "/a~1b"), Some(&serde_json::json!(2)));
    }

    #[test]
    fn openrouter_key_reader_shows_remaining_credits_or_no_limit() {
        let resolved = Config::default()
            .resolve(Choice::Reader(Reader::OpenrouterKey), Some("https://openrouter.ai/api/v1"))
            .unwrap();
        assert_eq!(resolved.url, "https://openrouter.ai/api/v1/key");
        assert_eq!(resolved.auth, Auth::ProfileApiKey);
        assert_eq!(resolved.mode, Mode::Balance);
        // Response shape from https://openrouter.ai/docs/api/reference/limits.
        let capped = serde_json::json!({"data": {"label": "k", "limit": 20, "limit_remaining": 12.5, "usage": 7.5, "is_free_tier": false}});
        assert_eq!(parse(&resolved, &capped, 1).unwrap().balance.as_deref(), Some("USD 12.50"));
        let unlimited = serde_json::json!({"data": {"label": "k", "limit": null, "limit_remaining": null, "usage": 3, "is_free_tier": false}});
        assert_eq!(parse(&resolved, &unlimited, 1).unwrap().balance.as_deref(), Some("no limit"));
    }

    #[test]
    fn moonshot_balance_uses_the_platform_currency() {
        let config = Config::default();
        // Response shape from https://platform.kimi.ai/docs/api/balance.
        let body = serde_json::json!({
            "code": 0,
            "data": {
                "available_balance": 49.58894,
                "voucher_balance": 46.58893,
                "cash_balance": 3.00001
            },
            "scode": "0x0",
            "status": true
        });
        let global = config.resolve(Choice::Reader(Reader::MoonshotBalance), Some("https://api.moonshot.ai/v1")).unwrap();
        let display = parse(&global, &body, 7).unwrap();
        assert_eq!(display.balance.as_deref(), Some("USD 49.59"));
        assert_eq!(display.label, "Kimi");
        assert_eq!((display.remaining_percent, display.reset_at, display.observed_at), (None, None, Some(7)));
        assert_eq!(gauge_kind(&global), "balance");

        let cn = config.resolve(Choice::Reader(Reader::MoonshotCnBalance), Some("https://api.moonshot.cn/v1")).unwrap();
        assert_eq!(parse(&cn, &body, 7).unwrap().balance.as_deref(), Some("¥49.59"));

        // A response that names its currency (DeepSeek) keeps it; other
        // balance URLs without one show the bare amount.
        let ds = config.resolve(Choice::Reader(Reader::DeepseekBalance), None).unwrap();
        let usd = serde_json::json!({"balance_infos": [{"currency": "USD", "total_balance": "2.5"}]});
        assert_eq!(parse(&ds, &usd, 1).unwrap().balance.as_deref(), Some("USD 2.50"));
        let other = Config { url: "https://example.com/balance".into(), ..cn.clone() };
        assert_eq!(parse(&other, &body, 1).unwrap().balance.as_deref(), Some("49.59"));

        // Missing or negative balances are errors (cached data is kept stale).
        assert!(parse(&cn, &serde_json::json!({"code": 1, "status": false}), 1).is_err());
        let negative = serde_json::json!({"data": {"available_balance": -1.0}});
        assert!(parse(&cn, &negative, 1).is_err());
    }

    #[test]
    fn balance_mode_preserves_currency_without_inventing_a_quota_bar() {
        let config = Config {
            enabled: true,
            url: "https://api.deepseek.com/user/balance".into(),
            mode: Mode::Balance,
            value_pointer: "/balance_infos/0/total_balance".into(),
            limit_pointer: String::new(),
            reset_pointer: String::new(),
            ..Config::default()
        };
        let display = parse(
            &config,
            &serde_json::json!({
                "balance_infos": [{
                    "currency": "CNY",
                    "total_balance": "83.215"
                }]
            }),
            1,
        )
        .unwrap();
        assert_eq!(display.balance.as_deref(), Some("¥83.22"));
        assert_eq!(display.remaining_percent, None);
        assert_eq!(display.reset_at, None);
    }
    #[test]
    fn extra_gauges_share_one_fetch_and_keep_old_serialization() {
        let dir = std::env::temp_dir().join(format!("cutex-quota-extra-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut a: StoredAccount = serde_json::from_value(
            serde_json::json!({"id":uuid::Uuid::new_v4().to_string(),"name":"glm"}),
        )
        .unwrap();
        // Backward compatibility: no extras and no window label serialize as before.
        let options = serde_json::to_value(&a.options).unwrap();
        assert!(options.get("quotas").is_none());
        assert!(options["quota"].get("window_label").is_none());

        a.options.quota = Config {
            enabled: true,
            url: "https://example.com/usage".into(),
            auth: Auth::None,
            label: "GLM".into(),
            window_label: Some("5h".into()),
            ..Config::default()
        };
        a.options.quotas = vec![
            Config {
                enabled: true,
                url: String::new(), // inherits the primary endpoint
                label: "GLMw".into(),
                window_label: Some("week".into()),
                value_pointer: "/rate_limits/1/remaining".into(),
                limit_pointer: "/rate_limits/1/limit".into(),
                reset_pointer: String::new(),
                ..Config::default()
            },
            Config {
                enabled: false,
                ..Config::default()
            },
        ];
        let calls = std::cell::Cell::new(0);
        let gauges = gauges_in(&a, true, &dir, |_, _| {
            calls.set(calls.get() + 1);
            Ok(serde_json::json!({"rate_limits":[
                {"remaining":30,"limit":100,"reset_at":"2026-10-06T10:00:00Z"},
                {"remaining":900,"limit":1000}
            ]}))
        })
        .unwrap();
        assert_eq!(calls.get(), 1, "gauges sharing a URL reuse one response");
        assert_eq!(gauges.len(), 2, "disabled extras are omitted");
        assert_eq!(gauges[0].id, "primary");
        assert_eq!(gauges[0].remaining_percent, Some(30.0));
        assert_eq!(gauges[0].window_label.as_deref(), Some("5h"));
        assert!(gauges[0].reset_at.is_some());
        assert_eq!(gauges[1].id, "extra-0");
        assert_eq!(gauges[1].label, "GLMw");
        assert_eq!(gauges[1].kind, "window");
        assert_eq!(gauges[1].remaining_percent, Some(90.0));
        assert_eq!(gauges[1].window_label.as_deref(), Some("week"));
        let json = serde_json::to_value(&gauges[1]).unwrap();
        assert_eq!(json["remainingPercent"], 90.0);
        assert!(json.get("windowLabel").is_some() && json.get("resetAt").is_some());

        // Cached afterwards: no fetch without refresh.
        let cached = gauges_in(&a, false, &dir, |_, _| panic!("cache expected")).unwrap();
        assert_eq!(cached[1].remaining_percent, Some(90.0));

        let mut bad = a.options.quota.clone();
        bad.window_label = Some("123456789".into());
        assert!(bad.validate().is_err());
        let round: StoredAccount =
            serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        assert_eq!(round.options.quotas.len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mapping_modes_and_missing_values() {
        let mut c = Config {
            value_pointer: "/used".into(),
            limit_pointer: "/cap".into(),
            reset_pointer: "".into(),
            mode: Mode::UsedAndLimit,
            ..Config::default()
        };
        assert_eq!(
            parse(&c, &serde_json::json!({"used":25,"cap":100}), 1)
                .unwrap()
                .remaining_percent,
            Some(75.0)
        );
        assert!(parse(&c, &serde_json::json!({"used":25,"cap":0}), 1).is_err());
        assert!(parse(&c, &serde_json::json!({"cap":100}), 1).is_err());
        c.mode = Mode::UsedPercent;
        assert_eq!(
            parse(&c, &serde_json::json!({"used":"120"}), 1)
                .unwrap()
                .remaining_percent,
            Some(0.0)
        );
        c.mode = Mode::RemainingPercent;
        assert_eq!(
            parse(&c, &serde_json::json!({"used":"42"}), 1)
                .unwrap()
                .remaining_percent,
            Some(42.0)
        );
        c.mode = Mode::UsedRatio;
        assert_eq!(parse(&c, &serde_json::json!({"used":0.25}), 1).unwrap().remaining_percent, Some(75.0));
        assert_eq!(parse(&c, &serde_json::json!({"used":"1.05"}), 1).unwrap().remaining_percent, Some(0.0));
        assert!(parse(&c, &serde_json::json!({"used":-0.5}), 1).is_err());
    }

    // ---- Kimi Code membership plan -----------------------------------------

    /// Legacy-plan `/usages` response: weekly `usage`, 5 h `limits[0]` and
    /// the ratio pools (shape of the official Kimi CLI / CodexBar fixtures;
    /// counts are strings, reset times carry nanoseconds).
    fn kimi_legacy_usages() -> Value {
        serde_json::json!({
            "user": {"membership": {"level": "LEVEL_BASIC"}},
            "usage": {"limit": "2048", "used": "512", "remaining": "1536",
                      "resetTime": "2026-10-20T15:23:13.373329235Z"},
            "limits": [{
                "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": "200", "used": "50", "remaining": "150",
                           "resetTime": "2026-10-16T15:05:24.374187075Z"}
            }],
            "usages": {
                "limit_5h": {"used_ratio": 0.25, "reset_time": "2026-10-16T15:05:24Z"},
                "limit_7d": {"used_ratio": 0.25, "reset_time": "2026-10-20T15:23:13Z"},
                "limit_month_total": {"used_ratio": 0.375,"reset_time": "2026-11-01T00:00:00Z"},
                "limit_month_code": {"used_ratio": 0.1, "reset_time": "2026-11-01T00:00:00Z"}
            }
        })
    }

    /// New-plan response: no weekly window (`usage` / `limit_7d` absent).
    fn kimi_new_plan_usages() -> Value {
        serde_json::json!({
            "limits": [{
                "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": 100, "used": 10, "remaining": 90, "resetTime": "2026-10-16T20:15:44Z"}
            }],
            "usages": {
                "limit_5h": {"used_ratio": 0.1, "reset_time": "2026-10-16T20:15:44Z"},
                "limit_month_total": {"used_ratio": 0.0056, "reset_time": "2026-10-17T00:00:00Z"}
            }
        })
    }

    /// A loopback HTTP server answering one request per `(status, body)`;
    /// returns its base URL and the request heads it saw (lowercased).
    fn fake_server(replies: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                seen.push(String::from_utf8(request).unwrap().to_lowercase());
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            seen
        });
        (format!("http://{addr}"), server)
    }

    /// A profile as `enable_preset_reader` leaves it for a Kimi Code base
    /// URL, but with the reader's resolved endpoint stored as custom (the
    /// test has no materialized profile config to read the base URL from).
    fn kimi_account(base: &str) -> StoredAccount {
        let mut a: StoredAccount = serde_json::from_value(
            serde_json::json!({"id": uuid::Uuid::new_v4().to_string(), "name": "kimi"}),
        )
        .unwrap();
        enable_preset_reader(&mut a, Some(Reader::KimiCodePlan), Some(base), None);
        assert_eq!(a.quota_reader, Some(Reader::KimiCodePlan));
        assert!(a.options.quota.enabled);
        a.quota_reader = None;
        a
    }

    const KIMI_AUTH: &[u8] = br#"{"OPENAI_API_KEY":"sk-kimi-fixture"}"#;

    #[test]
    fn kimi_code_reader_derives_usages_from_the_base_url() {
        let resolve = |base: Option<&str>| Config::default().resolve(Choice::Reader(Reader::KimiCodePlan), base);
        for (base, url) in [
            ("https://api.kimi.com/coding/v1", "https://api.kimi.com/coding/v1/usages"),
            ("https://api.kimi.com/coding/v1/", "https://api.kimi.com/coding/v1/usages"),
            ("https://api.kimi.ai/coding/v1", "https://api.kimi.ai/coding/v1/usages"),
            // Anthropic-compatible base (Claude Code setup) and a bare origin.
            ("https://api.kimi.com/coding/", "https://api.kimi.com/coding/v1/usages"),
            ("https://api.kimi.com", "https://api.kimi.com/coding/v1/usages"),
            ("http://127.0.0.1:9/coding/v1", "http://127.0.0.1:9/coding/v1/usages"),
        ] {
            let resolved = resolve(Some(base)).unwrap();
            assert_eq!(resolved.url, url, "{base}");
            assert_eq!(resolved.auth, Auth::ProfileApiKey);
            assert_eq!(resolved.mode, Mode::UsedAndLimit);
            assert_eq!(resolved.label, "Kimi");
            assert_eq!(resolved.window_label.as_deref(), Some("5h"));
            assert!(!resolved.optional);
        }
        assert!(resolve(None).unwrap_err().to_string().contains("needs the profile's base URL"));
        assert!(Reader::KimiCodePlan.needs_base_url());
        assert_eq!(Choice::parse_settable("kimi_code_plan"), Ok(Choice::Reader(Reader::KimiCodePlan)));
        assert!(Choice::settable_values().contains(&"kimi_code_plan"));

        let extras = Config::extras_for_new_profile(Reader::KimiCodePlan);
        let windows: Vec<_> = extras.iter().map(|c| (c.window_label.as_deref(), c.mode, c.optional, c.url.as_str())).collect();
        assert_eq!(windows, [(Some("week"), Mode::UsedAndLimit, true, ""), (Some("month"), Mode::UsedRatio, true, "")]);
        // New fields serialize only when set; `used_ratio` is the wire name.
        let month = serde_json::to_value(&extras[1]).unwrap();
        assert_eq!((month["mode"].as_str(), month["optional"].as_bool()), (Some("used_ratio"), Some(true)));
        assert!(serde_json::to_value(Config::default()).unwrap().get("optional").is_none());
        let round: Config = serde_json::from_value(month).unwrap();
        assert_eq!(round, extras[1]);
    }

    #[test]
    fn kimi_code_plan_reads_5h_week_and_month_from_a_fake_server() {
        let dir = std::env::temp_dir().join(format!("cutex-quota-kimi-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let (base, server) = fake_server(vec![
            (200, kimi_legacy_usages().to_string()),
            (200, kimi_new_plan_usages().to_string()),
        ]);
        let a = kimi_account(&format!("{base}/coding/v1"));
        let fetch = |c: &Config, _: &[u8]| fetch_json(c, KIMI_AUTH);

        let gauges = gauges_in(&a, true, &dir, fetch).unwrap();
        let summary: Vec<_> = gauges.iter().map(|g| (g.id.as_str(), g.window_label.as_deref(), g.remaining_percent)).collect();
        assert_eq!(
            summary,
            [("primary", Some("5h"), Some(75.0)), ("extra-0", Some("week"), Some(75.0)), ("extra-1", Some("month"), Some(62.5))]
        );
        assert!(gauges.iter().all(|g| g.label == "Kimi" && g.kind == "window" && !g.stale && g.error.is_none()));
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
        assert_eq!(gauges[0].reset_at, Some(at("2026-10-16T15:05:24Z")));
        assert_eq!(gauges[1].reset_at, Some(at("2026-10-20T15:23:13Z")));
        assert_eq!(gauges[2].reset_at, Some(at("2026-11-01T00:00:00Z")));

        // New plan in a fresh cache: the weekly window was never read, so it
        // is not reported; the 5 h and month windows are.
        let fresh = dir.join("fresh");
        let gauges = gauges_in(&a, true, &fresh, fetch).unwrap();
        let summary: Vec<_> = gauges.iter().map(|g| (g.id.as_str(), g.window_label.as_deref(), g.remaining_percent)).collect();
        assert_eq!(summary.len(), 2, "{summary:?}");
        assert_eq!(summary[0], ("primary", Some("5h"), Some(90.0)));
        assert_eq!(summary[1].0, "extra-1");
        assert!((summary[1].2.unwrap() - 99.44).abs() < 1e-9);

        // One request per read (the three gauges share it), with the key as
        // a Bearer token on the Kimi Code path.
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2);
        for request in &seen {
            assert!(request.starts_with("get /coding/v1/usages "), "{request}");
            assert!(request.contains("authorization: bearer sk-kimi-fixture\r\n"), "{request}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn kimi_code_plan_errors_leave_stale_gauges_and_name_the_failure() {
        let dir = std::env::temp_dir().join(format!("cutex-quota-kimi-err-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let (base, server) = fake_server(vec![
            (401, r#"{"error":{"message":"Invalid Authentication","type":"invalid_authentication_error"}}"#.into()),
            (429, r#"{"error":{"type":"rate_limit_reached_error"}}"#.into()),
            (200, "{not json".into()),
            (200, r#"{"limits":[{"detail":{"limit":"100","remaining":"99"}}]}"#.into()),
            (200, kimi_legacy_usages().to_string()),
            (401, "{}".into()),
        ]);
        let a = kimi_account(&format!("{base}/coding/v1"));
        let primary = a.options.quota.clone();
        let error = |expected: &str| {
            let err = fetch_json(&primary, KIMI_AUTH).map(|_| ()).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        };
        error("HTTP 401");
        error("HTTP 429");
        error("Invalid or oversized quota JSON");
        // A detail without `used` is a missing field, not a full window.
        let partial = fetch_json(&primary, KIMI_AUTH).unwrap();
        assert!(parse(&primary, &partial, 1).unwrap_err().to_string().contains("missing"));

        // Through the gauge reader: a success, then a refused refresh keeps
        // the last values marked stale (optional windows included).
        let fetch = |c: &Config, _: &[u8]| fetch_json(c, KIMI_AUTH);
        let ok = gauges_in(&a, true, &dir, fetch).unwrap();
        assert_eq!(ok.len(), 3);
        let refused = gauges_in(&a, true, &dir, fetch).unwrap();
        assert_eq!(refused.len(), 3);
        for (before, after) in ok.iter().zip(&refused) {
            assert!(after.stale && after.error.is_none(), "{after:?}");
            assert_eq!(after.remaining_percent, before.remaining_percent);
        }
        assert_eq!(server.join().unwrap().len(), 6);

        // A first read that fails: the primary shows an empty stale gauge,
        // the optional windows are not reported.
        let empty = gauges_in(&a, true, &dir.join("never"), |_, _| anyhow::bail!("Quota endpoint returned HTTP 401")).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!((empty[0].id.as_str(), empty[0].remaining_percent, empty[0].stale), ("primary", None, true));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
