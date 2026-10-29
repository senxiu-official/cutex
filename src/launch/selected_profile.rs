//! Typed selected-profile projection. Not a general TOML importer.
//! Credential values never belong to this serializable review domain.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::stock::VerifiedFile;
use crate::role_revision::Sha256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    ChatgptFile,
    /// Legacy API-key route of early projections with an implied relay
    /// provider. Still deserialized (persisted reviews), never produced and
    /// no longer launchable (`profile-generic-switches`: no default relay
    /// base URL): its runtimes re-review onto `ApiKey`.
    GlmApiKey,
    ApiKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCustody {
    pub path: PathBuf,
    pub parent_device: u64,
    pub parent_inode: u64,
    pub owner: u32,
    pub account: Option<Sha256>,
    // Opaque API keys have no independently verifiable account ID. A file
    // replacement/write needs review; no key bytes or key digest are retained.
    pub api_file: Option<super::job_mcp::PrivateObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_parent_file_id: Option<[u8; 16]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_api_file_id: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub version: Version,
    pub route: Route,
    pub auth: AuthCustody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiConnection>,
    pub settings: Settings,
    pub catalog: Option<VerifiedFile>,
    pub requires_job: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<super::selected_status::Status>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Version;
impl TryFrom<u8> for Version {
    type Error = &'static str;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        if v == 2 {
            Ok(Self)
        } else {
            Err("unsupported selected projection version")
        }
    }
}
impl From<Version> for u8 {
    fn from(_: Version) -> u8 {
        2
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_login_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_supports_reasoning_summaries: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_verbosity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_model: Option<String>,
    pub approvals_reviewer: Option<String>,
    pub service_tier: Option<String>,
    pub shell_environment_policy: Option<ShellPolicy>,
    #[serde(default)]
    pub projects: BTreeMap<String, Trust>,
    pub memories: Option<Memories>,
    pub model_auto_compact_token_limit: Option<u64>,
    pub skills: Option<Skills>,
    pub tui: Option<Tui>,
    pub notice: Option<Notice>,
    #[serde(default)]
    pub plugins: BTreeMap<String, Plugin>,
    /// Absent keeps the original reviewed serialization exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Tools>,
    /// Native `WebSearchMode` (Gemini relay profiles: "disabled"). Absent
    /// keeps the original reviewed serialization exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<String>,
    /// Native `ToolSchemaCompat` ("off" | "strict"; r53+, older natives
    /// ignore it). Absent keeps the original reviewed serialization exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_schema_compat: Option<String>,
    /// r250 reviewed the profile imageHistoryTrim keys here, and launch
    /// receipts persist these settings, which r249 and older refuse
    /// (`deny_unknown_fields`: rollback hazard H1). Still read, then
    /// dropped and never written again: the trim is a launch-time override
    /// ([`ImageHistoryTrimArgs`]).
    #[serde(default, skip_serializing, rename = "generated_image_history_max_inline")]
    pub r250_image_history_max_inline: Dropped,
    #[serde(default, skip_serializing, rename = "generated_image_history_keep")]
    pub r250_image_history_keep: Dropped,
}
/// A persisted field read only so that older records still load; its value
/// is discarded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped;
impl<'de> Deserialize<'de> for Dropped {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Dropped)
    }
}
/// Profile imageHistoryTrim (design 027 C) as native `-c` overrides, read
/// from the profile's `config.toml` each time the native is spawned (the
/// setting applies from an agent's next launch). Never part of the reviewed
/// [`Settings`]: launch receipts persist those, and r249 and older refuse
/// unknown keys there. A lone key is forwarded as is (the native decides).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageHistoryTrimArgs {
    pub max_inline: Option<u64>,
    pub keep: Option<u64>,
}
impl ImageHistoryTrimArgs {
    /// The keys of a profile `config.toml` (other keys are not looked at).
    pub fn from_toml(raw: &str) -> anyhow::Result<Self> {
        use crate::profiles::codex_profile::{IMAGE_HISTORY_KEEP_KEY, IMAGE_HISTORY_MAX_INLINE_KEY};
        let root: toml::Table = toml::from_str(raw).context("invalid profile config.toml")?;
        let key = |name: &str| -> anyhow::Result<Option<u64>> {
            root.get(name)
                .map(|value| {
                    value.as_integer().and_then(|v| u64::try_from(v).ok()).with_context(|| format!("invalid profile {name}"))
                })
                .transpose()
        };
        let trim = Self { max_inline: key(IMAGE_HISTORY_MAX_INLINE_KEY)?, keep: key(IMAGE_HISTORY_KEEP_KEY)? };
        trim.validate()?;
        Ok(trim)
    }
    /// The keys of the materialized profile `profile_id` (no file: none).
    pub fn of_profile(profile_id: &str) -> anyhow::Result<Self> {
        ensure!(uuid::Uuid::parse_str(profile_id).is_ok(), "invalid profile ID");
        let path = crate::profiles::materialize::materialized_profiles_dir()?.join(profile_id).join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(raw) => Self::from_toml(&raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("profile config.toml unreadable"),
        }
    }
    /// Each key in its API range; together max_inline must exceed keep.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.keep.is_none_or(|v| crate::profiles::codex_profile::IMAGE_HISTORY_KEEP_RANGE.contains(&v)),
            "invalid generated image history keep"
        );
        ensure!(
            self.max_inline.is_none_or(|v| {
                (2..=crate::profiles::codex_profile::IMAGE_HISTORY_MAX_INLINE_MAX).contains(&v)
                    && self.keep.is_none_or(|keep| v > keep)
            }),
            "invalid generated image history max inline"
        );
        Ok(())
    }
    pub fn args(&self) -> Vec<String> {
        use crate::profiles::codex_profile::{IMAGE_HISTORY_KEEP_KEY, IMAGE_HISTORY_MAX_INLINE_KEY};
        [(IMAGE_HISTORY_MAX_INLINE_KEY, self.max_inline), (IMAGE_HISTORY_KEEP_KEY, self.keep)]
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| ["-c".to_owned(), format!("{key}={v}")]))
            .flatten()
            .collect()
    }
}
/// Only the profile-scoped native tool switches; other `tools` keys stay in
/// the shared native config (the `-c tools=...` layer merges into it).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_plan: Option<ToolSwitch>,
}
impl Tools {
    /// An empty `[tools]` / `[tools.update_plan]` projects nothing.
    pub fn projected(tools: Option<Self>) -> Option<Self> {
        tools.filter(|tools| tools.update_plan.as_ref().is_some_and(|t| t.enabled.is_some()))
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSwitch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellPolicy {
    pub exclude: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    pub trust_level: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memories {
    pub generate_memories: bool,
    pub use_memories: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skills {
    pub config: Vec<Skill>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub enabled: bool,
    pub path: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResumeCwd {
    Session,
    Current,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tui {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cwd: Option<ResumeCwd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_line: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_line_use_colors: Option<bool>,
    /// Native owns presentation settings. Keep their values nested under tui;
    /// they do not select providers, credentials, tools or permissions.
    #[serde(default, flatten)]
    pub presentation: BTreeMap<String, serde_json::Value>,
    /// Native persisted tooltip counters, not model availability authority.
    /// Absent/empty preserves the original reviewed serialization exactly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_availability_nux: BTreeMap<String, u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_rate_limit_model_nudge: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub enabled: bool,
}

// Explicit fields, rather than flatten + arbitrary config values. An opted-in
// private materialization adds the mode; original profiles are never rewritten.
//
// Unknown keys (backend auto-update RFC §6.2 / §9, persistence audit): a
// root or `model_providers.<id>` key this binary does not know is IGNORED
// (never projected into a launch, listed in `ignored_keys`), so a key a newer
// cutex added (r233 `web_search`, r53 provider `capabilities`) no longer makes
// an older binary refuse the whole profile. Credential / command overrides
// stay refused explicitly ([`REFUSED_ROOT_KEYS`], [`REFUSED_PROVIDER_KEYS`]);
// the nested tables keep `deny_unknown_fields`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub windows: Option<BTreeMap<String, serde_json::Value>>,
    pub forced_login_method: Option<String>,
    pub model_context_window: Option<i64>,
    pub model_reasoning_summary: Option<String>,
    pub model_supports_reasoning_summaries: Option<bool>,
    pub model_verbosity: Option<String>,
    pub plan_mode_reasoning_effort: Option<String>,
    pub review_model: Option<String>,
    pub cutex_provider_mode: String,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub model_reasoning_effort: Option<String>,
    #[serde(default = "file_storage")]
    pub cli_auth_credentials_store: String,
    pub approvals_reviewer: Option<String>,
    pub service_tier: Option<String>,
    pub shell_environment_policy: Option<ShellPolicy>,
    #[serde(default)]
    pub projects: BTreeMap<String, Trust>,
    pub memories: Option<Memories>,
    pub model_auto_compact_token_limit: Option<u64>,
    pub model_catalog_json: Option<PathBuf>,
    pub skills: Option<Skills>,
    pub tui: Option<Tui>,
    pub notice: Option<Notice>,
    #[serde(default)]
    pub plugins: BTreeMap<String, Plugin>,
    pub tools: Option<Tools>,
    pub web_search: Option<String>,
    pub tool_schema_compat: Option<String>,
    pub generated_image_history_max_inline: Option<u64>,
    pub generated_image_history_keep: Option<u64>,
    #[serde(default)]
    pub model_providers: BTreeMap<String, ApiProvider>,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, LegacyJob>,
    /// Root / provider keys this binary does not know and ignored (dotted
    /// paths, sorted); filled by [`Config::parse`].
    #[serde(skip)]
    pub ignored_keys: Vec<String>,
}

/// Root keys that override credentials or run commands: always refused,
/// never silently ignored.
pub const REFUSED_ROOT_KEYS: &[&str] = &[
    "api_key",
    "notify",
    "experimental_bearer_token",
    "http_headers",
    "env_http_headers",
    "chatgpt_base_url",
];
/// `model_providers.<id>` keys that carry or redirect credentials.
pub const REFUSED_PROVIDER_KEYS: &[&str] =
    &["http_headers", "env_http_headers", "experimental_bearer_token", "api_key", "query_params"];
const KNOWN_ROOT_KEYS: &[&str] = &[
    "windows", "forced_login_method", "model_context_window", "model_reasoning_summary",
    "model_supports_reasoning_summaries", "model_verbosity", "plan_mode_reasoning_effort", "review_model",
    "cutex_provider_mode", "model", "model_provider", "model_reasoning_effort", "cli_auth_credentials_store",
    "approvals_reviewer", "service_tier", "shell_environment_policy", "projects", "memories",
    "model_auto_compact_token_limit", "model_catalog_json", "skills", "tui", "notice", "plugins", "tools",
    "web_search", "tool_schema_compat", "generated_image_history_max_inline", "generated_image_history_keep",
    "model_providers", "mcp_servers",
];
const KNOWN_PROVIDER_KEYS: &[&str] =
    &["name", "base_url", "wire_api", "requires_openai_auth", "env_key", "capabilities"];

/// Drop unknown root / provider keys (returned, sorted); refuse the
/// credential and command overrides.
fn strip_unknown_keys(root: &mut toml::Table) -> anyhow::Result<Vec<String>> {
    let mut ignored = Vec::new();
    for key in REFUSED_ROOT_KEYS {
        ensure!(!root.contains_key(*key), "selected profile key `{key}` is not allowed");
    }
    let unknown: Vec<String> = root.keys().filter(|key| !KNOWN_ROOT_KEYS.contains(&key.as_str())).cloned().collect();
    for key in unknown {
        root.remove(&key);
        ignored.push(key);
    }
    if let Some(toml::Value::Table(providers)) = root.get_mut("model_providers") {
        for (id, provider) in providers.iter_mut() {
            let toml::Value::Table(provider) = provider else { continue };
            for key in REFUSED_PROVIDER_KEYS {
                ensure!(!provider.contains_key(*key), "selected profile provider key `{key}` is not allowed");
            }
            let unknown: Vec<String> =
                provider.keys().filter(|key| !KNOWN_PROVIDER_KEYS.contains(&key.as_str())).cloned().collect();
            for key in unknown {
                provider.remove(&key);
                ignored.push(format!("model_providers.{id}.{key}"));
            }
        }
    }
    ignored.sort();
    Ok(ignored)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiProvider {
    pub name: String,
    pub base_url: String,
    #[serde(default = "responses_protocol")]
    pub wire_api: String,
    pub requires_openai_auth: bool,
    pub env_key: String,
    /// Native provider capability overrides (native r53+). Omitted unless the
    /// profile sets one, so stored projections of other profiles keep their
    /// exact bytes and an older cutex still reads them. Natives before r53 have
    /// no provider `capabilities` block; serde ignores it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ApiProviderCapabilities>,
}
/// `model_providers.<id>.capabilities` subset cutex forwards.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiProviderCapabilities {
    /// D4 (native r53): send base instructions in the top-level Responses
    /// `instructions` field (the pre-0.162 shape) instead of a `developer`
    /// input message. Native default false; only for a provider that rejects
    /// developer-role instructions. r52 and older always use the top-level field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_field: Option<bool>,
}
/// Base URL of the native's built-in `openai` provider.
pub const BUILTIN_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
fn file_storage() -> String { "file".into() }
fn responses_protocol() -> String { "responses".into() }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConnection {
    pub provider_id: String,
    pub provider: ApiProvider,
}
impl ApiConnection {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.provider_id.is_empty() && !self.provider_id.chars().any(char::is_control), "invalid provider id");
        let url = url::Url::parse(&self.provider.base_url)?;
        ensure!(matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
            && url.username().is_empty() && url.password().is_none(), "invalid API base URL");
        ensure!(self.provider.wire_api == "responses", "this runtime requires the Responses API protocol");
        ensure!(!self.provider.requires_openai_auth && self.provider.env_key == "OPENAI_API_KEY",
            "API-key profile must use its selected API credential file");
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyJob {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_vars: Vec<String>,
}

impl Config {
    /// Fill omitted native defaults without replacing explicit profile values.
    pub fn inherit_model_defaults(&mut self, shared: &toml::Value) {
        if self.model.is_none() {self.model=shared.get("model").and_then(toml::Value::as_str).map(str::to_owned);}
        if self.model_reasoning_effort.is_none() {self.model_reasoning_effort=shared.get("model_reasoning_effort").and_then(toml::Value::as_str).map(str::to_owned);}
    }
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let mut root: toml::Table =
            toml::from_str(raw).map_err(|_| anyhow::anyhow!("unsupported selected profile fields or types"))?;
        let ignored_keys = strip_unknown_keys(&mut root)?;
        let mut config: Self = toml::Value::Table(root)
            .try_into()
            .map_err(|_| anyhow::anyhow!("unsupported selected profile fields or types"))?;
        config.ignored_keys = ignored_keys;
        Ok(config)
    }
    pub fn review(
        self,
        id: &str,
        auth_path: PathBuf,
        selected_model: Option<&String>,
        selected_effort: Option<&String>,
    ) -> anyhow::Result<(Projection, String, Option<String>)> {
        ensure!(
            self.cutex_provider_mode == "selected_profile_v2",
            "explicit selected profile v2 required"
        );
        ensure!(
            self.cli_auth_credentials_store == "file",
            "selected profile requires File storage"
        );
        ensure!(uuid::Uuid::parse_str(id).is_ok(), "invalid profile ID");
        let auth_value: serde_json::Value = serde_json::from_slice(&bounded_asset(&auth_path)?)
            .map_err(|_| anyhow::anyhow!("invalid selected credential file"))?;
        let api_key = auth_value.get("OPENAI_API_KEY").and_then(serde_json::Value::as_str)
            .is_some_and(|key| !key.is_empty());
        let route = if api_key { Route::ApiKey } else { Route::ChatgptFile };
        let api = if api_key {
            let provider_id = self.model_provider.clone().unwrap_or_else(|| "openai".into());
            let provider = match self.model_providers.get(&provider_id) {
                Some(provider) => provider.clone(),
                None if provider_id == "openai" => ApiProvider {
                    name: "OpenAI".into(), base_url: BUILTIN_OPENAI_BASE_URL.into(),
                    wire_api: "responses".into(), requires_openai_auth: false, env_key: "OPENAI_API_KEY".into(),
                    capabilities: None,
                },
                None => anyhow::bail!("selected API provider definition missing"),
            };
            let connection = ApiConnection { provider_id, provider };
            connection.validate()?;
            Some(connection)
        } else {
            ensure!(self.model_provider.as_deref().is_none_or(|v| v == "openai")
                && self.model_providers.is_empty(),
                "subscription credentials require the native OpenAI subscription route");
            None
        };
        let model = selected_model
            .cloned()
            .or(self.model)
            .context("effective selected model missing")?;
        // An explicit effort must be offered by the model (validate_model
        // refuses it with the offered list); the profile default falls back
        // instead (docs/design/028), so a stale default never blocks a create.
        let profile_default = self.model_reasoning_effort.clone();
        let catalog = self
            .model_catalog_json
            .map(|path| -> anyhow::Result<VerifiedFile> {
                let path = if path.is_absolute() { path } else {
                    auth_path.parent().context("profile directory missing")?.join(path)
                }.canonicalize()?;
                validate_asset(&path)?;
                Ok(VerifiedFile {
                    sha256: crate::agent_management::file_sha256(&path)?,
                    path,
                })
            })
            .transpose()?;
        let effort = match selected_effort {
            Some(explicit) => Some(explicit.clone()),
            None => default_effort_for(&model, profile_default, catalog.as_ref())?,
        };
        validate_model(&route, &model, effort.as_deref(), catalog.as_ref())?;
        ensure!(
            self.mcp_servers.keys().all(|k| k == "cutex_job"),
            "unreviewed MCP server forbidden"
        );
        for job in self.mcp_servers.values() {
            ensure!(
                !job.command.is_empty() && job.args.len() <= 32 && job.env_vars.len() <= 8,
                "invalid legacy Job declaration"
            );
        }
        // Original declaration remains covered by the profile digest. No byte
        // of its command/args/env is executed; an independent reviewed Job
        // descriptor is mandatory at the root review boundary.
        let settings = Settings {
            windows: self.windows,
            forced_login_method: self.forced_login_method,
            model_context_window: self.model_context_window,
            model_reasoning_summary: self.model_reasoning_summary,
            model_supports_reasoning_summaries: self.model_supports_reasoning_summaries,
            model_verbosity: self.model_verbosity,
            plan_mode_reasoning_effort: self.plan_mode_reasoning_effort,
            review_model: self.review_model,

            approvals_reviewer: self.approvals_reviewer,
            service_tier: self.service_tier,
            shell_environment_policy: self.shell_environment_policy,
            projects: self.projects,
            memories: self.memories,
            model_auto_compact_token_limit: self.model_auto_compact_token_limit,
            skills: self.skills,
            tui: self.tui,
            notice: self.notice,
            plugins: self.plugins,
            tools: Tools::projected(self.tools),
            web_search: self.web_search,
            tool_schema_compat: self.tool_schema_compat,
            r250_image_history_max_inline: Dropped,
            r250_image_history_keep: Dropped,
        };
        settings.validate()?;
        // Applied at spawn ([`ImageHistoryTrimArgs::of_profile`]); a bad
        // value still fails the review.
        ImageHistoryTrimArgs {
            max_inline: self.generated_image_history_max_inline,
            keep: self.generated_image_history_keep,
        }
        .validate()?;
        let auth = review_auth(&auth_path, &route)?;
        Ok((
            Projection {
                version: Version,
                route,
                auth,
                api,
                settings,
                catalog,
                requires_job: !self.mcp_servers.is_empty(),
                status: None,
            },
            model,
            effort,
        ))
    }
}

impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.approvals_reviewer
                .as_deref()
                .is_none_or(|v| matches!(v, "user" | "auto_review" | "guardian_subagent")),
            "unsupported approval reviewer"
        );
        ensure!(
            self.service_tier.as_deref().is_none_or(|v| matches!(v, "default" | "fast" | "priority" | "flex")),
            "unsupported service tier"
        );
        ensure!(
            self.model_auto_compact_token_limit.is_none_or(|v| v > 0),
            "invalid compaction limit"
        );
        ensure!(
            self.web_search.as_deref().is_none_or(|v| {
                crate::profiles::codex_profile::WEB_SEARCH_VALUES.contains(&v)
            }),
            "unsupported web search mode"
        );
        ensure!(
            self.tool_schema_compat.as_deref().is_none_or(|v| {
                crate::profiles::codex_profile::TOOL_SCHEMA_COMPAT_VALUES.contains(&v)
            }),
            "unsupported tool schema compatibility mode"
        );
        // Trust keys are native configuration metadata, not launch paths.
        // A shared profile can retain keys from another operating system.
        for trust in self.projects.values() {
            ensure!(
                matches!(trust.trust_level.as_str(), "trusted" | "untrusted"),
                "invalid reviewed project trust"
            );
        }
        if let Some(shell) = &self.shell_environment_policy {
            ensure!(
                shell.exclude.len() <= 64
                    && shell
                        .exclude
                        .iter()
                        .all(|s| !s.is_empty() && !s.chars().any(char::is_control)),
                "invalid shell exclusions"
            );
        }
        if let Some(skills) = &self.skills {
            for skill in &skills.config {
                ensure!(
                    skill.path.is_absolute()
                        && !skill
                            .path
                            .components()
                            .any(|c| matches!(c, std::path::Component::ParentDir)),
                    "invalid skill declaration path"
                );
                // Preserve enabled and disabled intent. Native resolves skill
                // contents; this projection does not create or execute them.
            }
        }
        ensure!(
            self.plugins
                .keys()
                .all(|k| !k.trim().is_empty() && !k.chars().any(char::is_control)),
            "invalid plugin declaration key"
        );
        if let Some(tui) = &self.tui {
            ensure!(
                tui.model_availability_nux.len() <= 64
                    && tui.model_availability_nux.keys().all(|model| {
                        !model.is_empty()
                            && model.len() <= 128
                            && model.bytes().all(|b| {
                                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
                            })
                    }),
                "unsupported model tooltip counter keys"
            );
            ensure!(
                tui.status_line
                    .iter()
                    .flatten()
                    .all(|s| !s.starts_with("custom:")
                        || matches!(s.as_str(), "custom:profile" | "custom:bon-voyage" | "custom:notification")),
                "unsupported selected status item"
            );
        }
        Ok(())
    }
}

pub(super) fn validate_model_identifier(model: &str) -> anyhow::Result<()> {
    ensure!(
        !model.trim().is_empty() && !model.chars().any(char::is_control),
        "model identifier must be nonempty and contain no control characters"
    );
    Ok(())
}

pub(super) fn supported_effort(effort: &str) -> bool {
    matches!(
        effort,
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" | "persistent"
    )
}

/// A new agent's effort fell back from the profile default the model does
/// not offer (docs/design/028 (b)); reported in the agent_create reply.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EffortFallback {
    pub model: String,
    pub requested: String,
    pub used: String,
    /// `not_offered_by_model`.
    pub reason: String,
}

impl EffortFallback {
    /// Some when no effort was selected and the reviewed effort differs from
    /// the profile default (default_effort_for chose another level).
    pub fn of(model: &str, selected: Option<&String>, profile_default: Option<String>, used: Option<&str>) -> Option<Self> {
        let selected = selected.filter(|r| !crate::profiles::reasoning::is_default(r));
        let (None, Some(requested), Some(used)) = (selected, profile_default, used) else { return None };
        (requested != used).then(|| Self {
            model: model.to_owned(),
            requested,
            used: used.to_owned(),
            reason: "not_offered_by_model".into(),
        })
    }
}

/// The catalog row of `model` (exactly one), from a reviewed catalog file.
fn catalog_row(catalog: &VerifiedFile, model: &str) -> anyhow::Result<Option<serde_json::Value>> {
    catalog.validate()?;
    let bytes = bounded_asset(&catalog.path)?;
    use sha2::Digest;
    ensure!(format!("{:x}", sha2::Sha256::digest(&bytes)) == catalog.sha256.as_str(), "catalog changed during projection");
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid model catalog"))?;
    let models = v["models"].as_array().context("catalog models missing")?;
    let mut matched = models.iter().filter(|m| m["slug"].as_str() == Some(model));
    Ok(match (matched.next(), matched.next()) {
        (Some(row), None) => Some(row.clone()),
        _ => None,
    })
}

/// The efforts a catalog row offers, in its order.
pub fn row_efforts(row: &serde_json::Value) -> Vec<String> {
    row["supported_reasoning_levels"]
        .as_array()
        .map(|levels| levels.iter().filter_map(|l| l["effort"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// The effort a new agent runs with when the creator chose none: the
/// profile default when the model offers it (or the catalog lists no levels
/// / there is no catalog), else the row's own default, else its highest
/// level. Never refuses for a stale default (docs/design/028 (b)).
pub fn default_effort_for(
    model: &str,
    profile_default: Option<String>,
    catalog: Option<&VerifiedFile>,
) -> anyhow::Result<Option<String>> {
    let (Some(default), Some(catalog)) = (profile_default.clone(), catalog) else {
        return Ok(profile_default);
    };
    let Some(row) = catalog_row(catalog, model)? else {
        return Ok(Some(default));
    };
    Ok(Some(fallback_effort(&row, &default)))
}

/// `default` when `row` offers it (or offers nothing), else the row's
/// default level, else its highest level.
pub fn fallback_effort(row: &serde_json::Value, default: &str) -> String {
    let offered = row_efforts(row);
    if offered.is_empty() || offered.iter().any(|e| e == default) {
        return default.to_owned();
    }
    // GLM: the documented mapping (medium → high, xhigh / ultra → max, …).
    let glm = row["slug"].as_str().is_some_and(|slug| slug.starts_with("glm-"));
    let chosen = glm
        .then(|| crate::profiles::known_models::glm_effort(default))
        .flatten()
        .filter(|mapped| offered.iter().any(|e| e == mapped))
        .map(str::to_owned)
        .or_else(|| row["default_reasoning_level"].as_str().filter(|d| offered.iter().any(|e| e == d)).map(str::to_owned))
        .or_else(|| {
            const ORDER: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
            offered.iter().max_by_key(|e| ORDER.iter().position(|o| o == e)).cloned()
        })
        .unwrap_or_else(|| default.to_owned());
    eprintln!("selected profile: the profile default effort {default} is not offered by {}; using {chosen}", row["slug"].as_str().unwrap_or("?"));
    chosen
}

pub fn validate_model(
    route: &Route,
    model: &str,
    effort: Option<&str>,
    catalog: Option<&VerifiedFile>,
) -> anyhow::Result<()> {
    validate_model_identifier(model)?;
    match route {
        Route::ChatgptFile if catalog.is_none() => {
            ensure!(
                effort.is_none_or(supported_effort),
                "unsupported reasoning effort"
            );
        }
        Route::ChatgptFile | Route::GlmApiKey | Route::ApiKey => {
            let Some(catalog) = catalog else {
                ensure!(*route == Route::ApiKey, "legacy GLM catalog required");
                ensure!(effort.is_none_or(supported_effort), "unsupported reasoning effort");
                return Ok(());
            };
            catalog.validate()?;
            let bytes = bounded_asset(&catalog.path)?;
            use sha2::Digest;
            ensure!(
                format!("{:x}", sha2::Sha256::digest(&bytes)) == catalog.sha256.as_str(),
                "catalog changed during projection"
            );
            let v: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid model catalog"))?;
            let models = v["models"].as_array().context("catalog models missing")?;
            crate::profiles::model_variants::validate_bindings(models)?;
            let matched: Vec<_> = models
                .iter()
                .filter(|m| m["slug"].as_str() == Some(model))
                .collect();
            ensure!(
                matched.len() == 1
                    && matched[0]["supported_reasoning_levels"]
                        .as_array()
                        .is_some_and(|a| (*route != Route::GlmApiKey && (effort.is_none()
                            || (a.is_empty() && effort.is_none_or(supported_effort)))) || a
                            .iter()
                            .any(|v| v["effort"].as_str() == effort || (effort.is_none() && *route != Route::GlmApiKey))),
                "{}",
                if matched.len() == 1 {
                    format!(
                        "reasoning effort {} is not offered by model {model}; choose one of: {}",
                        effort.unwrap_or("(default)"),
                        row_efforts(matched[0]).join(", ")
                    )
                } else {
                    format!("catalog model/effort missing or ambiguous (model {model})")
                }
            );
        }
    }
    Ok(())
}

pub(super) fn bounded_asset(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut f = options.open(path)?;
    let before = f.metadata()?;
    ensure!(before.is_file(), "reviewed asset is not regular");
    ensure!(
        f.metadata()?.len() <= 4 * 1024 * 1024,
        "reviewed asset too large"
    );
    let mut bytes = Vec::new();
    (&mut f).take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let after = f.metadata()?;
        let named = std::fs::symlink_metadata(path)?;
        ensure!(
            before.dev() == named.dev()
                && before.ino() == named.ino()
                && before.len() == after.len()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
            "reviewed asset changed while reading"
        );
    }
    ensure!(bytes.len() <= 4 * 1024 * 1024, "reviewed asset grew");
    Ok(bytes)
}
pub(super) fn validate_asset(path: &Path) -> anyhow::Result<()> {
    super::stock::canonical(path)?;
    ensure!(path.is_file(), "reviewed asset missing");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0 && m.nlink() == 1,
            "reviewed asset custody invalid"
        );
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn review_auth(path: &Path, route: &Route) -> anyhow::Result<AuthCustody> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        path.is_absolute()
            && path.components().all(|c| matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "ordinary absolute auth path required"
    );
    let uid = unsafe { libc::geteuid() };
    for ancestor in path.ancestors().skip(1) {
        let m = std::fs::symlink_metadata(ancestor)?;
        ensure!(
            m.is_dir() && !m.file_type().is_symlink() && (m.uid() == 0 || m.uid() == uid),
            "untrusted auth ancestor"
        );
        ensure!(
            m.mode() & 0o022 == 0,
            "auth ancestor is group/other writable"
        );
    }
    let parent = std::fs::symlink_metadata(path.parent().context("auth parent missing")?)?;
    ensure!(
        parent.uid() == uid && parent.mode() & 0o7777 == 0o700,
        "auth parent must be owned0700"
    );
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.is_file() && m.uid() == uid && m.mode() & 0o7777 == 0o600 && m.nlink() == 1,
        "auth file must be owned regular0600 single link"
    );
    let (account, api_file) = match route {
        Route::ChatgptFile => (
            Some(super::chatgpt_auth_file::review(path.to_path_buf())?.account_sha256),
            None,
        ),
        Route::GlmApiKey | Route::ApiKey => {
            let _secret = read_api_key(path)?;
            (
                None,
                Some(super::job_mcp::PrivateObject {
                    device: m.dev(),
                    inode: m.ino(),
                    size: m.len(),
                    changed_seconds: m.ctime(),
                    changed_nanos: m.ctime_nsec(),
                }),
            )
        }
    };
    let parent_after = std::fs::symlink_metadata(path.parent().unwrap())?;
    ensure!(
        parent_after.dev() == parent.dev()
            && parent_after.ino() == parent.ino()
            && parent_after.mode() == parent.mode(),
        "auth parent changed"
    );
    Ok(AuthCustody {
        path: path.into(),
        parent_device: parent.dev(),
        parent_inode: parent.ino(),
        owner: uid,
        windows_parent_file_id: None,
        windows_api_file_id: None,
        account,
        api_file,
    })
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn review_auth(_: &Path, _: &Route) -> anyhow::Result<AuthCustody> {
    anyhow::bail!("selected auth requires Linux")
}

#[cfg(windows)]
#[path = "selected_profile_windows.rs"]
mod windows;
#[cfg(windows)]
pub use windows::review_auth;
#[cfg(windows)]
use windows::read_api_key;

/// Environment variable that carries an API-key profile's provider key into
/// the native runtime (every API provider's `env_key`).
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";

/// `-c` override that blanks the provider key in the environment native
/// builds for model-reachable tools (shell / unified exec / apply_patch /
/// code mode) and keeps it out of shell snapshots: native applies
/// `shell_environment_policy.set` after `inherit` and the excludes, and its
/// snapshot writer drops every `set` key from the captured exports. A dotted
/// `set.<key>` merges into a profile's own policy table (a
/// `-c shell_environment_policy.exclude=[...]` override would replace its
/// excludes). Native itself still reads the key from its own environment.
pub const API_KEY_SHELL_ENV_BLANK_OVERRIDE: &str = "shell_environment_policy.set.OPENAI_API_KEY";

/// The `-c` pair for [`API_KEY_SHELL_ENV_BLANK_OVERRIDE`].
pub fn api_key_shell_env_blank_args() -> [String; 2] {
    ["-c".into(), format!("{API_KEY_SHELL_ENV_BLANK_OVERRIDE}=\"\"")]
}

// Deliberately neither Debug nor Serialize. Used only immediately before child
// spawn, never inserted into LaunchCommand's printable plan.
pub struct ApiKey(String);
impl ApiKey {
    pub fn apply(&self, command: &mut std::process::Command) {
        command.env("OPENAI_API_KEY", &self.0);
    }
    pub fn as_exec_env(&self) -> anyhow::Result<std::ffi::CString> {
        std::ffi::CString::new(format!("OPENAI_API_KEY={}", self.0))
            .map_err(|_| anyhow::anyhow!("invalid API credential"))
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_api_key(path: &Path) -> anyhow::Result<ApiKey> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.len() <= 1024 * 1024
            && before.uid() == unsafe { libc::geteuid() }
            && before.mode() & 0o7777 == 0o600
            && before.nlink() == 1,
        "invalid API credential custody"
    );
    let mut bytes = Vec::new();
    (&mut file).take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        before.dev() == named.dev()
            && before.ino() == named.ino()
            && before.len() == after.len()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "API credential changed while reading"
    );
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid API credential JSON"))?;
    ensure!(
        v.as_object().is_some_and(|o| o.keys().all(|k| matches!(
            k.as_str(),
            "OPENAI_API_KEY" | "auth_mode" | "tokens" | "last_refresh"
        ))) && v
            .get("auth_mode")
            .is_none_or(|m| m.is_null() || m.as_str() == Some("apikey"))
            && v.get("tokens").is_none_or(|v| v.is_null())
            && v.get("last_refresh").is_none_or(|v| v.is_null()
                || v.as_str()
                    .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())),
        "unsupported API credential fields"
    );
    let key = v["OPENAI_API_KEY"]
        .as_str()
        .filter(|k| !k.is_empty() && !k.chars().any(char::is_control))
        .context("API credential missing")?;
    Ok(ApiKey(key.into()))
}
impl Projection {
    /// The API route uses the native's built-in OpenAI provider (no provider
    /// definition of the profile's own).
    pub fn uses_builtin_openai_provider(&self) -> bool {
        self.api.as_ref().is_some_and(|api| {
            api.provider_id == "openai" && api.provider.base_url.trim_end_matches('/') == BUILTIN_OPENAI_BASE_URL
        })
    }
    pub fn provider_id(&self) -> &str {
        self.api.as_ref().map(|api| api.provider_id.as_str())
            .unwrap_or(if self.route == Route::GlmApiKey { "GLM" } else { "openai" })
    }
    /// The same nonsecret argv projection is used by owner launch and the
    /// independent process test. Remote attach deliberately omits auth-file.
    pub fn native_args(&self, owner: bool) -> anyhow::Result<Vec<String>> {
        self.validate_for_launch(owner)?;
        let mut args = Vec::new();
        if owner {
            args.extend([
                "--auth-file".into(),
                self.auth
                    .path
                    .to_str()
                    .context("auth path must be UTF-8")?
                    .into(),
            ]);
        }
        let mut option = |key: &str, value: serde_json::Value| -> anyhow::Result<()> {
            let value = toml::Value::try_from(value)?;
            args.extend(["-c".into(), format!("{key}={value}")]);
            Ok(())
        };
        option("cli_auth_credentials_store", serde_json::json!("file"))?;
        if let Some(api) = &self.api {
            api.validate()?;
            option("model_providers", serde_json::to_value(BTreeMap::from([
                (&api.provider_id, &api.provider)
            ]))?)?;
        }
        let settings = serde_json::to_value(&self.settings)?;
        for (key, value) in settings.as_object().context("typed settings missing")? {
            // The existing remote owner also owns its approval reviewer.
            if !owner && key == "approvals_reviewer" {
                continue;
            }
            if !value.is_null() && !value.as_object().is_some_and(|v| v.is_empty()) {
                option(key, value.clone())?;
            }
        }
        if let Some(catalog) = &self.catalog {
            option(
                "model_catalog_json",
                serde_json::json!(catalog
                    .path
                    .to_str()
                    .context("catalog path must be UTF-8")?),
            )?;
        }
        // API-key routes hand the key to native as `OPENAI_API_KEY` (the
        // provider's `env_key`). Keep it out of tool subprocesses and shell
        // snapshots; after the typed settings so their policy table stays.
        if matches!(self.route, Route::GlmApiKey | Route::ApiKey) {
            args.extend(api_key_shell_env_blank_args());
        }
        Ok(args)
    }
    /// Maintenance moves the native user skill root but not the original
    /// profile. Preserve exact enabled/disabled intent at the copied location.
    /// No path is discovered from names or rendered text.
    pub fn migrated_native_args(
        &self,
        owner: bool,
        source_home: &Path,
        destination: &Path,
    ) -> anyhow::Result<Vec<String>> {
        let mut args = self.native_args(owner)?;
        if let Some(skills) = &self.settings.skills {
            let mut mapped = skills.clone();
            for skill in &mut mapped.config {
                if let Ok(relative) = skill.path.strip_prefix(source_home.join("skills")) {
                    skill.path = destination.join("skills").join(relative);
                }
            }
            let value = toml::Value::try_from(&mapped)?;
            args.extend(["-c".into(), format!("skills={value}")]);
        }
        Ok(args)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_for_launch(true)
    }
    fn validate_for_launch(&self, owner: bool) -> anyhow::Result<()> {
        ensure!(
            self.route != Route::GlmApiKey,
            "legacy relay API-key projection is no longer launchable; a fresh review uses the profile's own provider"
        );
        self.settings.validate()?;
        ensure!(self.api.is_some() == (self.route == Route::ApiKey), "API connection and credential route mismatch");
        if let Some(api) = &self.api { api.validate()?; }
        let wants_status = self.settings.tui.as_ref().is_some_and(|t| {
            t.status_line
                .iter()
                .flatten()
                .any(|s| super::selected_status::is_static_id(s))
        });
        ensure!(
            wants_status == self.status.is_some(),
            "selected status projection missing or inappropriate"
        );
        if let Some(status) = &self.status {
            if owner { status.validate()?; } else { status.validate_frozen()?; }
        }
        ensure!(
            review_auth(&self.auth.path, &self.route)? == self.auth,
            "selected auth custody/account changed"
        );
        if let Some(c) = &self.catalog {
            validate_asset(&c.path)?;
            c.validate()?;
        }
        Ok(())
    }
    pub fn secret(&self) -> anyhow::Result<Option<ApiKey>> {
        self.validate()?;
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        if matches!(self.route, Route::GlmApiKey | Route::ApiKey) {
            let key = read_api_key(&self.auth.path)?;
            ensure!(
                review_auth(&self.auth.path, &self.route)? == self.auth,
                "API credential changed before spawn"
            );
            return Ok(Some(key));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn omitted_profile_model_inherits_home_but_explicit_values_win() {
        let shared:toml::Value=toml::from_str("model='gpt-6-astra'\nmodel_reasoning_effort='high'").unwrap();
        let mut c=Config::parse("cutex_provider_mode='selected_profile_v2'").unwrap();
        c.inherit_model_defaults(&shared);
        assert_eq!(c.model.as_deref(),Some("gpt-6-astra"));
        assert_eq!(c.model_reasoning_effort.as_deref(),Some("high"));
        let mut explicit=Config::parse("cutex_provider_mode='selected_profile_v2'\nmodel='glm-5'\nmodel_reasoning_effort='low'").unwrap();
        explicit.inherit_model_defaults(&shared);
        assert_eq!(explicit.model.as_deref(),Some("glm-5"));
        assert_eq!(explicit.model_reasoning_effort.as_deref(),Some("low"));
    }

    #[test]
    fn shared_trust_keys_preserve_both_platforms_without_becoming_launch_paths() {
        let config = Config::parse(r#"cutex_provider_mode="selected_profile_v2"
[projects.'/home/example/work']
trust_level = "trusted"
[projects.'E:\work']
trust_level = "untrusted"
"#).unwrap();
        let settings = Settings { projects: config.projects, ..Default::default() };
        settings.validate().unwrap();
        assert_eq!(settings.projects.len(), 2);
        assert_eq!(settings.projects[r"E:\work"].trust_level, "untrusted");
        assert_eq!(settings.projects["/home/example/work"].trust_level, "trusted");
    }
    #[test]
    fn profile_update_plan_switch_is_projected_as_a_native_tools_override() {
        let base = "cutex_provider_mode='selected_profile_v2'\n";
        for (extra, expected) in [
            ("[tools.update_plan]\nenabled=true\n", Some(true)),
            ("[tools.update_plan]\nenabled=false\n", Some(false)),
            ("[tools.update_plan]\n", None),
            ("[tools]\n", None),
            ("", None),
        ] {
            let config = Config::parse(&format!("{base}{extra}")).unwrap();
            let tools = Tools::projected(config.tools);
            let settings = Settings { tools, ..Default::default() };
            settings.validate().unwrap();
            let value = serde_json::to_value(&settings).unwrap();
            match expected {
                Some(enabled) => {
                    assert_eq!(value["tools"], serde_json::json!({"update_plan": {"enabled": enabled}}));
                    // native_args renders each typed setting as `-c key=value`;
                    // native merges the CLI layer into the shared `tools` table.
                    let rendered = toml::Value::try_from(value["tools"].clone()).unwrap();
                    let reparsed: toml::Value = toml::from_str(&format!("tools={rendered}")).unwrap();
                    assert_eq!(reparsed["tools"]["update_plan"]["enabled"].as_bool(), Some(enabled));
                }
                None => assert!(value.get("tools").is_none(), "{extra:?}: {value}"),
            }
            // The persisted projection round-trips.
            assert_eq!(serde_json::from_value::<Settings>(value).unwrap(), settings);
        }
        // Other tool keys are not part of the reviewed profile projection.
        for extra in ["[tools]\nweb_search=true\n", "[tools.update_plan]\nenabled='yes'\n",
                      "[tools.update_plan]\nenabled=true\nother=1\n"] {
            assert!(Config::parse(&format!("{base}{extra}")).is_err(), "{extra}");
        }
    }

    #[test]
    fn profile_web_search_mode_is_projected_as_a_native_override() {
        let base = "cutex_provider_mode='selected_profile_v2'\n";
        let config = Config::parse(&format!("{base}web_search='disabled'\n")).unwrap();
        let settings = Settings { web_search: config.web_search, ..Default::default() };
        settings.validate().unwrap();
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["web_search"], "disabled");
        let rendered = toml::Value::try_from(value["web_search"].clone()).unwrap();
        assert_eq!(format!("web_search={rendered}"), "web_search=\"disabled\"");
        assert_eq!(serde_json::from_value::<Settings>(value).unwrap(), settings);
        // Unset keeps the reviewed serialization unchanged.
        let unset = Settings::default();
        assert!(serde_json::to_value(&unset).unwrap().get("web_search").is_none());
        let bad = Settings { web_search: Some("everywhere".into()), ..Default::default() };
        assert!(bad.validate().is_err());
        assert!(Config::parse(&format!("{base}web_search=false\n")).is_err());
    }

    #[test]
    fn profile_tool_schema_compat_is_projected_as_a_native_override() {
        let base = "cutex_provider_mode='selected_profile_v2'\n";
        let config = Config::parse(&format!("{base}tool_schema_compat='strict'\n")).unwrap();
        assert!(config.ignored_keys.is_empty(), "{:?}", config.ignored_keys);
        let settings = Settings { tool_schema_compat: config.tool_schema_compat, ..Default::default() };
        settings.validate().unwrap();
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["tool_schema_compat"], "strict");
        let rendered = toml::Value::try_from(value["tool_schema_compat"].clone()).unwrap();
        assert_eq!(format!("tool_schema_compat={rendered}"), "tool_schema_compat=\"strict\"");
        assert_eq!(serde_json::from_value::<Settings>(value).unwrap(), settings);
        // Unset keeps the reviewed serialization unchanged.
        assert!(serde_json::to_value(Settings::default()).unwrap().get("tool_schema_compat").is_none());
        let bad = Settings { tool_schema_compat: Some("gemini".into()), ..Default::default() };
        assert!(bad.validate().is_err());
        assert!(Config::parse(&format!("{base}tool_schema_compat=true\n")).is_err());
    }

    #[test]
    fn profile_image_history_trim_is_a_launch_override_not_a_reviewed_setting() {
        let base = "cutex_provider_mode='selected_profile_v2'\n";
        let raw = format!("{base}generated_image_history_max_inline=12\ngenerated_image_history_keep=4\n");
        let config = Config::parse(&raw).unwrap();
        assert!(config.ignored_keys.is_empty(), "{:?}", config.ignored_keys);
        assert_eq!((config.generated_image_history_max_inline, config.generated_image_history_keep), (Some(12), Some(4)));
        let trim = ImageHistoryTrimArgs::from_toml(&raw).unwrap();
        assert_eq!(trim, ImageHistoryTrimArgs { max_inline: Some(12), keep: Some(4) });
        assert_eq!(
            trim.args(),
            ["-c", "generated_image_history_max_inline=12", "-c", "generated_image_history_keep=4"]
        );
        // A lone key is forwarded as is; none = no override.
        assert_eq!(
            ImageHistoryTrimArgs::from_toml(&format!("{base}generated_image_history_keep=4\n")).unwrap().args(),
            ["-c", "generated_image_history_keep=4"]
        );
        assert!(ImageHistoryTrimArgs::from_toml(base).unwrap().args().is_empty());
        for (max_inline, keep) in [(Some(4), Some(4)), (Some(501), Some(4)), (Some(12), Some(0)), (Some(80), Some(51)), (Some(1), None)] {
            assert!(ImageHistoryTrimArgs { max_inline, keep }.validate().is_err(), "{max_inline:?} {keep:?}");
        }
        for bad in ["generated_image_history_keep='4'", "generated_image_history_keep=-1", "generated_image_history_max_inline=4\ngenerated_image_history_keep=4"] {
            assert!(ImageHistoryTrimArgs::from_toml(&format!("{base}{bad}\n")).is_err(), "{bad}");
        }
        assert!(Config::parse(&format!("{base}generated_image_history_keep='4'\n")).is_err());
        assert!(Config::parse(&format!("{base}generated_image_history_keep=-1\n")).is_err());
    }

    /// The reviewed `Settings` of r249 (b54585c), the oldest rollback target:
    /// its field names, `deny_unknown_fields`. Values are not checked here.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct R249Settings {
        windows: Option<serde::de::IgnoredAny>,
        forced_login_method: Option<serde::de::IgnoredAny>,
        model_context_window: Option<serde::de::IgnoredAny>,
        model_reasoning_summary: Option<serde::de::IgnoredAny>,
        model_supports_reasoning_summaries: Option<serde::de::IgnoredAny>,
        model_verbosity: Option<serde::de::IgnoredAny>,
        plan_mode_reasoning_effort: Option<serde::de::IgnoredAny>,
        review_model: Option<serde::de::IgnoredAny>,
        approvals_reviewer: Option<serde::de::IgnoredAny>,
        service_tier: Option<serde::de::IgnoredAny>,
        shell_environment_policy: Option<serde::de::IgnoredAny>,
        projects: Option<serde::de::IgnoredAny>,
        memories: Option<serde::de::IgnoredAny>,
        model_auto_compact_token_limit: Option<serde::de::IgnoredAny>,
        skills: Option<serde::de::IgnoredAny>,
        tui: Option<serde::de::IgnoredAny>,
        notice: Option<serde::de::IgnoredAny>,
        plugins: Option<serde::de::IgnoredAny>,
        tools: Option<serde::de::IgnoredAny>,
        web_search: Option<serde::de::IgnoredAny>,
        tool_schema_compat: Option<serde::de::IgnoredAny>,
    }

    #[test]
    fn receipts_keep_the_r249_settings_shape_and_r250_records_still_load() {
        // What a review of a profile with every projected key (and the
        // trim) persists: r249 must read it.
        let config = Config::parse(concat!(
            "cutex_provider_mode='selected_profile_v2'\n",
            "forced_login_method='chatgpt'\nmodel_context_window=200000\nmodel_reasoning_summary='auto'\n",
            "model_supports_reasoning_summaries=true\nmodel_verbosity='low'\nplan_mode_reasoning_effort='high'\n",
            "review_model='gpt-5'\napprovals_reviewer='user'\nservice_tier='fast'\n",
            "model_auto_compact_token_limit=100000\nweb_search='disabled'\ntool_schema_compat='strict'\n",
            "generated_image_history_max_inline=20\ngenerated_image_history_keep=5\n",
            "[windows]\nsandbox='unelevated'\n[shell_environment_policy]\nexclude=['X']\n",
            "[projects.'/w']\ntrust_level='trusted'\n[notice]\nhide_rate_limit_model_nudge=true\n",
            "[plugins.'p@x']\nenabled=true\n[tools.update_plan]\nenabled=false\n",
        ))
        .unwrap();
        let settings = Settings {
            windows: config.windows,
            forced_login_method: config.forced_login_method,
            model_context_window: config.model_context_window,
            model_reasoning_summary: config.model_reasoning_summary,
            model_supports_reasoning_summaries: config.model_supports_reasoning_summaries,
            model_verbosity: config.model_verbosity,
            plan_mode_reasoning_effort: config.plan_mode_reasoning_effort,
            review_model: config.review_model,
            approvals_reviewer: config.approvals_reviewer,
            service_tier: config.service_tier,
            shell_environment_policy: config.shell_environment_policy,
            projects: config.projects,
            model_auto_compact_token_limit: config.model_auto_compact_token_limit,
            notice: config.notice,
            plugins: config.plugins,
            tools: Tools::projected(config.tools),
            web_search: config.web_search,
            tool_schema_compat: config.tool_schema_compat,
            ..Default::default()
        };
        settings.validate().unwrap();
        let written = serde_json::to_value(&settings).unwrap();
        assert!(written.get("generated_image_history_max_inline").is_none(), "{written}");
        assert!(written.get("generated_image_history_keep").is_none(), "{written}");
        serde_json::from_value::<R249Settings>(written.clone()).expect("r249 reads the receipt settings");
        assert_eq!(serde_json::from_value::<Settings>(written).unwrap(), settings);
        // r250 wrote the trim into receipts: still loaded, the keys dropped,
        // equal to a fresh review, and written back in the r249 shape.
        let mut r250 = serde_json::to_value(&settings).unwrap();
        r250["generated_image_history_max_inline"] = serde_json::json!(20);
        r250["generated_image_history_keep"] = serde_json::json!(5);
        assert!(serde_json::from_value::<R249Settings>(r250.clone()).is_err(), "the r249 replica must deny unknown keys");
        let loaded: Settings = serde_json::from_value(r250).unwrap();
        assert_eq!(loaded, settings);
        serde_json::from_value::<R249Settings>(serde_json::to_value(&loaded).unwrap()).unwrap();
        // Other unknown keys are still refused.
        let mut unknown = serde_json::to_value(&settings).unwrap();
        unknown["future_key"] = serde_json::json!(1);
        assert!(serde_json::from_value::<Settings>(unknown).is_err());
    }

    #[test]
    fn native_windows_options_are_accepted_and_preserved() {
        let config = Config::parse("cutex_provider_mode='selected_profile_v2'\n[windows]\nsandbox='unelevated'\n").unwrap();
        let settings = Settings { windows: config.windows, ..Default::default() };
        settings.validate().unwrap();
        assert_eq!(serde_json::to_value(&settings).unwrap()["windows"], serde_json::json!({"sandbox":"unelevated"}));
    }

    #[test]
    fn native_presentation_is_preserved_without_inventing_status_defaults() {
        let original = serde_json::json!({
            "animations": false, "show_tooltips": true, "theme": "night-owl",
            "terminal_title": ["activity", "project"],
            "keymap": {"global": {"copy": "ctrl+c"}}, "resume_cwd": "session",
        });
        let tui: Tui = serde_json::from_value(original.clone()).unwrap();
        assert!(tui.status_line.is_none());
        assert!(tui.status_line_use_colors.is_none());
        assert_eq!(serde_json::to_value(&tui).unwrap(), original);
        let settings = Settings {
            tui: Some(tui),
            ..Default::default()
        };
        settings.validate().unwrap();
        let minimal: Tui = toml::from_str("animations=false").unwrap();
        assert_eq!(toml::to_string(&minimal).unwrap(), "animations = false\n");
        let invalid: Result<Tui, _> = toml::from_str("animations=false\nresume_cwd='other'");
        assert!(invalid.is_err());
        let mut native = settings.clone();
        native.tui.as_mut().unwrap().status_line = Some(vec!["git-branch".into()]);
        native.validate().unwrap();
        native.tui.as_mut().unwrap().status_line = Some(vec!["custom:unknown".into()]);
        assert!(native.validate().is_err());
    }

    #[test]
    fn native_resume_cwd_preference_is_accepted_and_preserved() {
        for value in ["session", "current"] {
            let config = format!(
                "status_line = []\nstatus_line_use_colors = true\nresume_cwd = \"{value}\"\n"
            );
            let tui: Tui = toml::from_str(&config).unwrap();
            let encoded = toml::to_string(&tui).unwrap();
            assert!(encoded.contains(&format!("resume_cwd = \"{value}\"")));
        }
        assert!(toml::from_str::<Tui>(
            "status_line=[]\nstatus_line_use_colors=true\nresume_cwd='other'"
        )
        .is_err());
    }

    #[test]
    fn selected_models_allow_new_names_and_native_efforts_without_fallback() {
        for model in ["gpt-6-astra", "gpt-5.6-luna", "future-model/revision-2"] {
            assert!(validate_model(&Route::ChatgptFile, model, None, None).is_ok());
            for effort in [
                "none",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
                "persistent",
            ] {
                assert!(validate_model(&Route::ChatgptFile, model, Some(effort), None).is_ok());
            }
        }
        for (model, effort) in [
            ("", Some("low")),
            ("bad\nmodel", Some("low")),
            ("gpt-5.6-sol", Some("guessed")),
        ] {
            assert!(validate_model(&Route::ChatgptFile, model, effort, None).is_err());
        }
        assert!(validate_model(&Route::GlmApiKey, "glm-5.3", Some("max"), None).is_err());
    }
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn new_account_identity_uses_configured_route_and_forwards_enabled_settings() {
        use base64::Engine;
        use std::os::unix::fs::PermissionsExt;
        // Auth validation intentionally rejects /tmp and group-writable source
        // ancestors. Keep this disposable fixture in the actual user's home,
        // independently of the test harness's redirected HOME.
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 16384];
        assert_eq!(
            unsafe {
                libc::getpwuid_r(
                    libc::geteuid(),
                    &mut passwd,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            },
            0
        );
        assert!(!result.is_null());
        let home = unsafe { std::ffi::CStr::from_ptr(passwd.pw_dir) }
            .to_str()
            .unwrap();
        let root = Path::new(home).join(format!(".cutex-profile-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let auth = root.join("auth.json");
        let id_token = format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"https://api.openai.com/auth":{"chatgpt_user_id":"fixture-user"}}"#)
        );
        std::fs::write(&auth, serde_json::to_vec(&serde_json::json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,
            "tokens":{"id_token":id_token,"access_token":"fixture-access","refresh_token":"fixture-refresh","account_id":"fixture-account"},"last_refresh":null})).unwrap()).unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = "cutex_provider_mode='selected_profile_v2'\ncli_auth_credentials_store='file'\nmodel='future-model/revision-2'\n[[skills.config]]\npath='/private/my-skill/SKILL.md'\nenabled=true\n[plugins.'custom@tools']\nenabled=true\n";
        let (projection, model, effort) = Config::parse(config)
            .unwrap()
            .review(&uuid::Uuid::new_v4().to_string(), auth, None, None)
            .unwrap();
        assert_eq!(projection.route, Route::ChatgptFile);
        assert_eq!(model, "future-model/revision-2");
        assert_eq!(effort, None);
        let args = projection.native_args(true).unwrap().join(" ");
        // Subscription routes carry no provider key in the environment.
        assert!(!args.contains(API_KEY_SHELL_ENV_BLANK_OVERRIDE));
        assert!(args.contains("custom@tools"));
        assert!(args.contains("/private/my-skill/SKILL.md"));
        assert!(args.contains("enabled = true"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn glm_model_selection_uses_supplied_catalog_instead_of_hardcoded_slug() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("selected-glm-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("catalog.json");
        std::fs::write(
            &path,
            br#"{"models":[{"slug":"glm-next","supported_reasoning_levels":[{"effort":"max"}]}]}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let file = VerifiedFile {
            sha256: crate::agent_management::file_sha256(&path).unwrap(),
            path,
        };
        assert!(validate_model(&Route::GlmApiKey, "glm-next", Some("max"), Some(&file)).is_ok());
        assert!(
            validate_model(&Route::GlmApiKey, "missing-model", Some("max"), Some(&file)).is_err()
        );
        assert!(validate_model(&Route::GlmApiKey, "glm-next", Some("high"), Some(&file)).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn effort_fallback_is_only_reported_for_a_replaced_default() {
        let of = |selected: Option<&str>, default: Option<&str>, used: Option<&str>| {
            EffortFallback::of("glm-5.3", selected.map(str::to_owned).as_ref(), default.map(str::to_owned), used)
        };
        assert_eq!(of(None, Some("max"), Some("high")).unwrap().reason, "not_offered_by_model");
        assert_eq!(of(Some("default"), Some("max"), Some("high")).unwrap().used, "high");
        assert!(of(None, Some("high"), Some("high")).is_none());
        assert!(of(Some("high"), Some("max"), Some("high")).is_none());
        assert!(of(None, None, Some("high")).is_none());
    }

    /// Zooey (10-22): profile default `max`, glm-5.3 offers low / medium /
    /// high. No explicit effort → the row's default, never a refusal; an
    /// explicit unsupported effort is refused with the offered list.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_stale_profile_default_effort_falls_back_but_an_explicit_one_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("selected-effort-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("catalog.json");
        std::fs::write(&path, br#"{"models":[
            {"slug":"glm-5.3","default_reasoning_level":"medium","supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]},
            {"slug":"glm-5.3-flash","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]},
            {"slug":"plain","supported_reasoning_levels":[]}]}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let file = VerifiedFile { sha256: crate::agent_management::file_sha256(&path).unwrap(), path };
        let fall = |model: &str, default: Option<&str>| default_effort_for(model, default.map(str::to_owned), Some(&file)).unwrap();
        assert_eq!(fall("glm-5.3", Some("max")).as_deref(), Some("medium"));
        // GLM mapping first: xhigh → max is not offered here, so the row default.
        assert_eq!(fall("glm-5.3", Some("xhigh")).as_deref(), Some("medium"));
        assert_eq!(fall("glm-5.3-flash", Some("medium")).as_deref(), Some("high"));
        assert_eq!(fall("glm-5.3-flash", Some("minimal")).as_deref(), Some("low"));
        assert_eq!(fall("glm-5.3-flash", Some("max")).as_deref(), Some("high"));
        assert_eq!(fall("glm-5.3", Some("high")).as_deref(), Some("high"));
        assert_eq!(fall("plain", Some("max")).as_deref(), Some("max"));
        assert_eq!(fall("unknown", Some("max")).as_deref(), Some("max"));
        assert_eq!(fall("glm-5.3", None), None);
        assert!(validate_model(&Route::ApiKey, "glm-5.3", Some("medium"), Some(&file)).is_ok());
        let refused = validate_model(&Route::ApiKey, "glm-5.3", Some("max"), Some(&file)).unwrap_err().to_string();
        assert!(refused.contains("not offered by model glm-5.3") && refused.contains("low, medium, high"), "{refused}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_profile_ignores_unknown_keys_but_refuses_credential_overrides() {
        let base = "cutex_provider_mode='selected_profile_v2'\ncli_auth_credentials_store='file'\n";
        assert!(Config::parse(base).is_ok());
        // A key a newer cutex added: ignored (not projected), never fatal.
        let newer = Config::parse(&format!(
            "future_switch=true\n{base}[model_providers.GLM]\nname='GLM'\nbase_url='https://example.invalid/v1'\nrequires_openai_auth=false\nenv_key='OPENAI_API_KEY'\nfuture_cap={{x=1}}\n"
        ))
        .unwrap();
        assert_eq!(newer.ignored_keys, vec!["future_switch".to_string(), "model_providers.GLM.future_cap".to_string()]);
        assert_eq!(newer.model_providers["GLM"].name, "GLM");
        // Known keys keep their types; nested tables stay strict.
        assert!(Config::parse(&format!("{base}web_search=3")).is_err());
        assert!(Config::parse(&format!("{base}[tools]\nunknown_tool={{enabled=true}}")).is_err());
        for extra in ["experimental_bearer_token='x'", "chatgpt_base_url='https://x.invalid'"] {
            assert!(Config::parse(&format!("{base}{extra}")).is_err(), "{extra}");
        }
        for extra in ["api_key='sentinel'", "notify=['shell']", "[model_providers.GLM]\nname='GLM'\nbase_url='https://relay.example/v1'\nwire_api='responses'\nrequires_openai_auth=false\nenv_key='OPENAI_API_KEY'\nhttp_headers={Authorization='sentinel'}", "[shell_environment_policy]\nset={SECRET='sentinel'}"] {
            assert!(Config::parse(&format!("{base}{extra}")).is_err());
        }
        for version in [0, 1, 3, 255] {
            assert!(serde_json::from_value::<Version>(version.into()).is_err());
        }
        assert_eq!(serde_json::to_value(Version).unwrap(), serde_json::json!(2));
    }
    #[test]
    fn settings_preserve_enabled_and_disabled_skills_and_arbitrary_plugins() {
        let mut s = Settings::default();
        s.skills = Some(Skills {
            config: vec![Skill {
                enabled: false,
                path: std::env::temp_dir().join("missing/SKILL.md"),
            }],
        });
        s.plugins
            .insert("sample@debug".into(), Plugin { enabled: true });
        assert!(s.validate().is_ok());
        s.skills.as_mut().unwrap().config[0].enabled = true;
        s.plugins
            .insert("custom@tools".into(), Plugin { enabled: false });
        assert!(s.validate().is_ok());
        s.plugins.insert("".into(), Plugin { enabled: true });
        assert!(s.validate().is_err());
        s.plugins.remove("");
        s.skills = None;
        s.tui = Some(Tui {
            resume_cwd: Some(ResumeCwd::Session),
            status_line: Some(vec!["custom:profile".into()]),
            status_line_use_colors: Some(true),
            presentation: BTreeMap::new(),
            model_availability_nux: BTreeMap::new(),
        });
        assert!(s.validate().is_ok());
        s.tui
            .as_mut()
            .unwrap()
            .status_line
            .as_mut()
            .unwrap()
            .push("custom:unknown".into());
        assert!(s.validate().is_err());
    }
    #[test]
    fn tooltip_counters_preserve_native_values_without_changing_legacy_bytes() {
        let old =
            serde_json::json!({"status_line":["custom:profile"],"status_line_use_colors":true});
        let mut tui: Tui = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(&tui).unwrap(), old);
        tui.model_availability_nux.insert("gpt-5.5".into(), 2);
        tui.model_availability_nux.insert("gpt-5.6-sol".into(), 1);
        let mut settings = Settings {
            tui: Some(tui),
            ..Default::default()
        };
        assert!(settings.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&settings).unwrap()["tui"]["model_availability_nux"]["gpt-5.5"],
            2
        );
        settings
            .tui
            .as_mut()
            .unwrap()
            .model_availability_nux
            .insert("bad/key".into(), 1);
        assert!(settings.validate().is_err());
        let mut malformed = old;
        malformed["model_availability_nux"] = serde_json::json!({"gpt-5.5":-1});
        assert!(serde_json::from_value::<Tui>(malformed).is_err());
    }
    #[test]
    fn native_service_tiers_and_reviewers_are_supported() {
        for tier in ["default", "fast", "priority", "flex"] {
            Settings { service_tier: Some(tier.into()), ..Default::default() }.validate().unwrap();
        }
        for reviewer in ["user", "auto_review", "guardian_subagent"] {
            Settings { approvals_reviewer: Some(reviewer.into()), ..Default::default() }.validate().unwrap();
        }
        assert!(Settings { service_tier: Some("invalid".into()), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn actual_catalog_reference_is_pinned_not_remote_claim() {
        // Fixture source identity is independently checked by the protocol test.
        assert_eq!(
            super::super::stock::S6E_SERVER_COMMIT,
            "2eab060b191a0fe59e22b28785d02d76eafb7fc4"
        );
        assert!(Settings {
            approvals_reviewer: Some("auto".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
    }
    #[test]
    fn api_connections_accept_provider_names_and_endpoints_without_brand_fences() {
        for (id, endpoint) in [("GLM", "https://api.z.ai/api/coding/paas/v4"),
            ("relay", "https://api.example-relay.com/v1"), ("deepseek", "https://api.deepseek.com/v1"),
            ("local", "http://127.0.0.1:8080/v1"), ("openai", "https://api.openai.com/v1")] {
            let connection = ApiConnection { provider_id: id.into(), provider: ApiProvider {
                name:id.into(), base_url:endpoint.into(), wire_api:"responses".into(),
                requires_openai_auth:false, env_key:"OPENAI_API_KEY".into(), capabilities: None } };
            connection.validate().unwrap();
        }
        assert!(validate_model(&Route::ApiKey, "manual-model", None, None).is_ok());
        assert!(validate_model(&Route::ApiKey, "manual-model", Some("high"), None).is_ok());
    }

    /// The `-c model_providers=...` value `native_args` emits for a provider.
    fn provider_override(id: &str, provider: &ApiProvider) -> String {
        let value = toml::Value::try_from(serde_json::to_value(BTreeMap::from([(id, provider)])).unwrap()).unwrap();
        format!("model_providers={value}")
    }

    #[test]
    fn provider_instructions_field_capability_is_forwarded_only_when_set() {
        let base = "cutex_provider_mode='selected_profile_v2'\nmodel='m'\nmodel_provider='relay'\n\
            [model_providers.relay]\nname='relay'\nbase_url='http://127.0.0.1:9/v1'\n\
            wire_api='responses'\nrequires_openai_auth=false\nenv_key='OPENAI_API_KEY'\n";
        let plain = Config::parse(base).unwrap().model_providers.remove("relay").unwrap();
        assert_eq!(plain.capabilities, None);
        let plain_arg = provider_override("relay", &plain);
        assert!(!plain_arg.contains("capabilities"), "{plain_arg}");
        // Stored projections without the field keep their bytes.
        assert!(!serde_json::to_string(&plain).unwrap().contains("capabilities"));

        let with = Config::parse(&format!("{base}[model_providers.relay.capabilities]\ninstructions_field=true\n"))
            .unwrap().model_providers.remove("relay").unwrap();
        assert_eq!(with.capabilities, Some(ApiProviderCapabilities { instructions_field: Some(true) }));
        let arg = provider_override("relay", &with);
        let parsed: toml::Value = toml::from_str(&arg).unwrap();
        assert_eq!(parsed["model_providers"]["relay"]["capabilities"]["instructions_field"].as_bool(), Some(true));
        // Other native capability keys are not forwarded blindly.
        assert!(Config::parse(&format!("{base}[model_providers.relay.capabilities]\nnamespace_tools=true\n")).is_err());
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn api_key_route_blanks_provider_key_for_native_tools_after_profile_policy() {
        use std::os::unix::fs::PermissionsExt;
        // Same custody rules as above: a disposable 0700 dir in the real home.
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 16384];
        assert_eq!(unsafe { libc::getpwuid_r(libc::geteuid(), &mut passwd,
            buffer.as_mut_ptr().cast(), buffer.len(), &mut result) }, 0);
        assert!(!result.is_null());
        let home = unsafe { std::ffi::CStr::from_ptr(passwd.pw_dir) }.to_str().unwrap();
        let root = Path::new(home).join(format!(".cutex-profile-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let auth = root.join("auth.json");
        std::fs::write(&auth, br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fixture-not-a-key"}"#).unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = "cutex_provider_mode='selected_profile_v2'\ncli_auth_credentials_store='file'\n\
            model='manual-model'\nmodel_provider='relay'\n\
            [model_providers.relay]\nname='relay'\nbase_url='http://127.0.0.1:9/v1'\n\
            wire_api='responses'\nrequires_openai_auth=false\nenv_key='OPENAI_API_KEY'\n\
            [shell_environment_policy]\nexclude=['AWS_*']\n";
        let (projection, _, _) = Config::parse(config).unwrap()
            .review(&uuid::Uuid::new_v4().to_string(), auth.clone(), None, None).unwrap();
        assert_eq!(projection.route, Route::ApiKey);
        for owner in [true, false] {
            let args = projection.native_args(owner).unwrap();
            assert!(!args.iter().any(|arg| arg.contains("sk-fixture")), "no key in argv");
            let blank = args.iter().position(|arg| arg == "shell_environment_policy.set.OPENAI_API_KEY=\"\"")
                .expect("API-key route blanks the provider key for tools");
            assert_eq!(args[blank - 1], "-c");
            // The profile's own table replaces the whole policy; the dotted
            // override must come after it to survive.
            let table = args.iter().position(|arg| arg.starts_with("shell_environment_policy="))
                .expect("profile policy forwarded");
            assert!(table < blank);
        }
        // The value parses as native's TOML override: an empty string.
        let value: toml::Value = toml::from_str(&api_key_shell_env_blank_args()[1]).unwrap();
        assert_eq!(value["shell_environment_policy"]["set"]["OPENAI_API_KEY"].as_str(), Some(""));
        std::fs::remove_dir_all(root).unwrap();
    }
}
