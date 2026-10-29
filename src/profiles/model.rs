//! Profile, account, auth, and global cutex config data models.

use std::collections::HashMap;

use anyhow::anyhow;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

pub const STORE_VERSION: u32 = 3;

/// A persisted Management API root credential whose diagnostic form never
/// reveals the credential bytes.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ManagementApiToken(String);

impl ManagementApiToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ManagementApiToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ManagementApiToken([REDACTED])")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CliKind {
    Codex,
    Claude,
    Dsh,
    /// r266 Kimi Code CLI (`kimi acp`; docs/development/r266-kimi-profiles.md).
    Kimi,
}

impl Default for CliKind {
    fn default() -> Self {
        CliKind::Codex
    }
}

impl std::fmt::Display for CliKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliKind::Codex => write!(f, "codex"),
            CliKind::Claude => write!(f, "claude"),
            CliKind::Dsh => write!(f, "dsh"),
            CliKind::Kimi => write!(f, "kimi"),
        }
    }
}

impl std::str::FromStr for CliKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "codex" => Ok(CliKind::Codex),
            "claude" => Ok(CliKind::Claude),
            "dsh" => Ok(CliKind::Dsh),
            "kimi" => Ok(CliKind::Kimi),
            _ => Err(anyhow!(
                "unknown CLI kind: {s} (expected 'codex', 'claude', 'dsh' or 'kimi')"
            )),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AccountsStore {
    pub version: u32,
    pub accounts: Vec<StoredAccount>,
    pub active_account_id: Option<String>,
}

impl Default for AccountsStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            accounts: Vec::new(),
            active_account_id: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StoredAccount {
    #[serde(default)]
    pub options: super::appearance::ProfileOptions,
    /// Browser tools are explicit opt-in for both runtimes. Keep this outside
    /// `options` (deny_unknown_fields) so pre-browser binaries can read the
    /// store, ignoring this additive field. Older saves may drop the opt-in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub browser_tools: bool,
    /// `web_search` cutex tool (profile-search-tool): one separate,
    /// search-only Responses request to this profile's provider / model, for
    /// providers that refuse the built-in web search mixed with function
    /// tools. Owner opt-in per profile, default off; no preset sets it. Top
    /// level for the same reason as `browser_tools`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub search_tool: bool,
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    /// Display-only override; empty falls back to imported `plan_type`.
    #[serde(default)]
    pub plan_label: Option<String>,
    /// Provider-aware endpoint preset applied above the custom quota mapping.
    #[serde(default)]
    pub quota_preset: super::quota::Preset,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    #[serde(default)]
    pub session: Option<SessionConfig>,
    #[serde(default)]
    pub cli_kind: CliKind,
    #[serde(default)]
    pub default_cli_args: Vec<String>,
    #[serde(default)]
    pub agent_name: Option<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    /// Provider category v2 key (`profiles::provider_category`), kept next
    /// to the legacy `options.provider` so r216 and older still read the
    /// store (they ignore this field and drop it when they save).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_category: Option<String>,
    /// Claude profiles: the Claude account (`runtime::claude_accounts`)
    /// new sessions on this profile run on; absent = `default` (the shared
    /// login home). Top level, not in `options` (`deny_unknown_fields`), so
    /// an older binary ignores it and a store without accounts is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_account: Option<String>,
    /// Per-entry display names of this profile's models (model id -> name;
    /// `profile-model-display-names`). Display only: never sent to a
    /// provider or written into a model catalog. Top level, not in `options`
    /// (`deny_unknown_fields`), so an older binary ignores it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub model_display_names: std::collections::BTreeMap<String, String>,
    /// profile-relay-runtimes: the owner's address of a provider interface
    /// that has no other home (interface kind -> base URL, e.g.
    /// `deepseekNative` -> a relay's Anthropic-compatible root). Absent =
    /// the interface's official default (`profiles::interface_urls`). Top
    /// level for the same reason as `browser_tools`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub interface_base_urls: std::collections::BTreeMap<String, String>,
    /// Explicit quota reader (`profile-generic-switches`): a named reader the
    /// owner picked (`quota::Reader`), resolved from the profile's base URL /
    /// fixed official endpoint. Absent = the custom `options.quota`. The
    /// legacy `quota_preset` field stays `custom` next to it (older binaries
    /// cannot read a new value there) and `options.quota` holds the resolved
    /// endpoint, so a binary without readers keeps reading the same gauge.
    /// Top level for the same reason as `browser_tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_reader: Option<super::quota::Reader>,
    /// Codex profiles: launch with the host's private native ChatGPT
    /// `auth.json` (explicit file custody, token refresh owned by native)
    /// instead of a profile credential (`profile-generic-switches`, PATCH
    /// `privateChatgptAuthFile`). Default off. Top level for the same reason
    /// as `browser_tools`; excluded from the stock launch account digest
    /// (the reviewed auth in the launch configuration already carries it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private_chatgpt_auth_file: bool,
    /// 「参与自动选择」 (design 041, `profile-auto-pick`): the owner lets
    /// `profile: "auto:<providerFamily>"` pick this profile for a new
    /// agent. Default off. Top level for the same reason as
    /// `browser_tools`; an older binary drops it on save (back to off).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_pick: bool,
    /// Claude profiles: Claude Code settings sources and claude.ai
    /// connectors (`profiles::claude_code`). Top level for the same reason
    /// as `browser_tools`.
    #[serde(default, skip_serializing_if = "super::claude_code::ClaudeCodeOptions::is_empty")]
    pub claude_code: super::claude_code::ClaudeCodeOptions,    /// r265 runtime variants and credential source/version
    /// (`profiles::runtime_variants`). Flattened top-level additive fields,
    /// each omitted when unset, so legacy profiles serialize unchanged.
    #[serde(flatten)]
    pub variants: super::runtime_variants::VariantState,
}

impl StoredAccount {
    pub fn from_import(name: String, snapshot: &ImportedSnapshot, runtime: RuntimeConfig) -> Self {
        Self {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            auto_pick: false,
            claude_code: Default::default(),
            variants: Default::default(),
            id: Uuid::new_v4().to_string(),
            name,
            email: snapshot.email.clone(),
            plan_type: snapshot.plan_type.clone(),
            plan_label: None,
            quota_preset: super::quota::Preset::default(),
            source: Some(snapshot.source.clone()),
            runtime,
            proxy: None,
            session: None,
            cli_kind: CliKind::default(),
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
            interface_base_urls: Default::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LegacyAccountsStoreV2 {
    #[serde(default)]
    pub accounts: Vec<LegacyStoredAccountV2>,
    pub active_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LegacyStoredAccountV2 {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub raw_auth_json: Option<String>,
    #[serde(default)]
    pub raw_config_toml: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthData>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    #[serde(default)]
    pub session: Option<SessionConfig>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RuntimeConfig {
    Host,
    Docker {
        image: String,
        #[serde(default)]
        user_name: Option<String>,
    },
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::Host
    }
}

pub fn runtime_label(runtime: &RuntimeConfig) -> &'static str {
    match runtime {
        RuntimeConfig::Host => "host",
        RuntimeConfig::Docker { .. } => "docker",
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum AuthData {
    ApiKey {
        key: String,
    },
    ChatGPT {
        id_token: String,
        access_token: String,
        refresh_token: String,
        account_id: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct QuickRunState {
    pub last_global_profile: Option<String>,
    pub per_directory: HashMap<String, String>,
}

/// Defaults scoped to a profile; absence follows the profile's own configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewSessionDefaults {
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSettings {
    #[serde(default = "default_true")]
    pub owner_may_pass_asks: bool,
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self { owner_may_pass_asks: true }
    }
}

impl QueueSettings {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Preserve an explicitly present JSON null as well as objects/scalars. An
/// ordinary Option<Value> decoder would turn null into absence on the next save.
fn deserialize_raw_asks<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CodezConfig {
    /// Raw delegation settings (`asks.limits`). AskLimits owns lenient parsing
    /// and defaults; typed config saves must retain this extension verbatim as
    /// a JSON value, including unknown fields and invalid values. Do not make
    /// malformed asks settings invalidate the unrelated typed configuration.
    #[serde(default, deserialize_with = "deserialize_raw_asks", skip_serializing_if = "Option::is_none")]
    pub asks: Option<serde_json::Value>,
    #[serde(default)]
    pub image_input: crate::management::v2::user_input::image_steering::ImageInputSettings,
    #[serde(default, skip_serializing_if = "QueueSettings::is_default")]
    pub queue: QueueSettings,
    #[serde(default)]
    pub peers: crate::peers::policy::Settings,
    #[serde(default)]
    pub appearance: super::appearance::Appearance,
    #[serde(default)]
    #[serde(skip)]
    pub docker_use_sudo: bool,
    #[serde(default)]
    pub custom_status_items: Vec<CustomStatusItemCatalogEntry>,
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    /// Claude Code CLI for new Claude sessions. The bundled CLI is the
    /// self-contained default; `CUTEX_CLAUDE_CLI` still overrides it.
    #[serde(default, skip_serializing_if = "crate::config::claude::ClaudeCliChoice::is_default")]
    pub claude_cli: crate::config::claude::ClaudeCliChoice,
    #[serde(default = "default_session_config")]
    pub session: SessionConfig,
    /// Claude usage polls use the app-server control plane; apply to newly launched runtimes.
    #[serde(default = "default_claude_usage_refresh_minutes")]
    pub claude_usage_refresh_minutes: u8,
    /// Background refresh interval for native (ChatGPT) profile quota
    /// snapshots, in minutes; 0 disables the background refresh.
    #[serde(default = "default_profile_quota_refresh_minutes")]
    pub profile_quota_refresh_minutes: u8,
    /// Management `cutex/session/retire` permanently deletes a session that
    /// never received user input instead of archiving it.
    #[serde(default = "default_archive_delete_empty")]
    pub archive_delete_empty: bool,
    /// Quick chats without a name get a generated title after their first
    /// message (a hidden ephemeral turn, as the TUI does).
    #[serde(default = "default_true")]
    pub auto_thread_titles: bool,
    /// Codex agents with their own native home launch with the shared
    /// home's `[plugins]`, `[mcp_servers]`, `skills.config` and
    /// `hooks.state` layered over the snapshot copied when they were created
    /// (`launch::shared_config`). Stored before the generalisation as
    /// `codex_plugins_follow_shared`, still read.
    #[serde(default = "default_true", alias = "codex_plugins_follow_shared")]
    pub codex_config_follow_shared: bool,
    /// Model of those titles; `None` = gpt-5.6-luna on a ChatGPT login when
    /// the catalog has it, else the session's own model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_title_model: Option<String>,
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub default_profile_direct_launch: bool,
    /// Profile for quick chats created without one (`POST /v2/human/sessions`);
    /// `None` falls back to `default_profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_chat_default_profile: Option<String>,
    #[serde(default)]
    pub new_session_defaults: std::collections::BTreeMap<String, NewSessionDefaults>,
    /// Launch permission defaults for new agents / quick chats (owned sessions).
    #[serde(default, skip_serializing_if = "crate::config::new_thread::NewThreadDefaults::is_empty")]
    pub new_thread_defaults: crate::config::new_thread::NewThreadDefaults,
    #[serde(default)]
    pub new_session_notification: crate::notify::session::Level,
    /// Frontend-only opt-in for terminals whose TrueColor marker is lost over SSH.
    #[serde(default)]
    pub terminal_truecolor: bool,
    /// Start Agents and Sessions with the local host selected.
    #[serde(default)]
    pub default_local_host_filter: bool,
    /// Legacy TethysUNE service controller, used only when no runtime-host is published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_service_controller: Option<String>,
    #[serde(default)]
    pub agent_sort: super::list_preferences::ListSort,
    #[serde(default)]
    pub session_sort: super::list_preferences::ListSort,
    #[serde(default)]
    pub project_sort: super::list_preferences::ListSort,
    #[serde(default)]
    pub job_sort: super::list_preferences::ListSort,

    #[serde(default)]
    #[serde(skip)]
    pub notify_service_url: Option<String>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_token: Option<String>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_idle_timeout_secs: Option<u64>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_composer_idle_timeout_secs: Option<u64>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_approval_timeout_secs: Option<u64>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_startup_idle_timeout_secs: Option<u64>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_events: Option<Vec<String>>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_user_message_content: Option<String>,
    #[serde(default)]
    #[serde(skip)]
    pub notify_service_user_message_preview_chars: Option<u64>,
    #[serde(default)]
    #[serde(skip)]
    pub rate_limit_threshold_warning_mode: Option<String>,
    #[serde(default)]
    #[serde(skip)]
    pub rate_limit_model_nudge_mode: Option<String>,
    #[serde(default)]
    #[serde(skip)]
    pub desktop_notify_enabled: bool,
    #[serde(default)]
    #[serde(skip)]
    pub desktop_notify_port: Option<u16>,
    #[serde(default)]
    #[serde(skip)]
    pub desktop_notify_token: Option<String>,
    #[serde(default = "default_true")]
    pub agent_bus_enabled: bool,
    #[serde(default)]
    pub agent_bus_port: Option<u16>,
    #[serde(default)]
    pub agent_bus_token: Option<String>,
    /// Agent Bus audit log rotation / coalescing (`agent_bus_audit`).
    #[serde(default, skip_serializing_if = "crate::config::agent_bus_audit::AgentBusAuditSettings::is_default")]
    pub agent_bus_audit: crate::config::agent_bus_audit::AgentBusAuditSettings,
    /// Agent Bus delivery settings (`agent_bus`: `wake_busy_wait_secs`,
    /// r217h1; typed in r220, still lenient: a wrong-typed value reads as the
    /// default and never breaks the rest of the config).
    #[serde(default, skip_serializing_if = "crate::config::agent_bus::AgentBusSettings::is_default")]
    pub agent_bus: crate::config::agent_bus::AgentBusSettings,
    /// Independent instance identity (`CUTEX_HOME`, docs/INSTANCES.md);
    /// absent in the default home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<crate::config::instance::InstanceSettings>,
    /// Port of `cutex management serve` and of every local client of this
    /// home; absent = 24270 (required in an instance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub management_port: Option<u16>,
    /// Explicit private-only opt-in; absent never creates display obligations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_job_presentation: Option<crate::app_server::presentation::PrivateJobPolicy>,
    #[serde(default)]
    pub management_api_token: Option<ManagementApiToken>,
    /// Agent cooperation boundary (project isolation, links). Authentication
    /// itself is not configurable; see `config::agent_security`.
    #[serde(
        default,
        skip_serializing_if = "crate::config::agent_security::AgentSecurityConfig::is_default"
    )]
    pub agent_security: crate::config::agent_security::AgentSecurityConfig,
    /// Uploaded session attachments (size cap, retention).
    #[serde(
        default,
        skip_serializing_if = "crate::config::attachments::AttachmentSettings::is_default"
    )]
    pub attachments: crate::config::attachments::AttachmentSettings,
    /// Input overflow → documents (render margin, quota, retention).
    #[serde(default, skip_serializing_if = "crate::config::documents::DocumentSettings::is_default")]
    pub documents: crate::config::documents::DocumentSettings,
    /// Usage cost estimates: price library, display currency, exchange
    /// rates (docs/usage-pricing.md). r216 and older ignore and drop it.
    #[serde(default, skip_serializing_if = "crate::config::usage_pricing::UsageSettings::is_default")]
    pub usage: crate::config::usage_pricing::UsageSettings,
    /// Management listen addresses (`"127.0.0.1:24270"`, `"<ip>"`,
    /// `"tailscale"`); absent = the `--bind` default. Non-loopback entries
    /// accept only client / device tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub management_listen: Option<Vec<String>>,
    /// CORS allow-list for the Management API; absent = localhost origins
    /// of Capacitor / WebView shells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub management_cors_origins: Option<Vec<String>>,
    /// Agent -> owner messages (`owner_notify`): channels per level, tool
    /// audience, rate limits, per-agent overrides.
    #[serde(default, skip_serializing_if = "crate::config::owner_notify::OwnerNotifySettings::is_default")]
    pub owner_notify: crate::config::owner_notify::OwnerNotifySettings,
    /// Notification levels (`notification_levels`): event → level and
    /// level → methods presets / changed cells, project mutes. Absent =
    /// legacy (r216 behaviour).
    #[serde(
        default,
        skip_serializing_if = "crate::config::notification_levels::NotificationLevelSettings::is_default"
    )]
    pub notification_levels: crate::config::notification_levels::NotificationLevelSettings,
    /// Session store maintenance (`session_store`): explicit-launch receipt
    /// retention (`receiptRetentionDays`).
    #[serde(default, skip_serializing_if = "crate::config::session_store::SessionStoreSettings::is_default")]
    pub session_store: crate::config::session_store::SessionStoreSettings,
    /// The `# Cutex` launch section (`agent_harness`, `enabled`). r217 and
    /// older ignore and drop it.
    #[serde(default, skip_serializing_if = "crate::config::agent_harness::AgentHarnessSettings::is_default")]
    pub agent_harness: crate::config::agent_harness::AgentHarnessSettings,
    /// Opt-in stable Responses prompt cache key for native agent launches
    /// (design 034, `promptCacheKey` in `/v2/settings`). Default off.
    #[serde(default, skip_serializing_if = "crate::config::prompt_cache_key::PromptCacheKeySettings::is_default")]
    pub prompt_cache_key: crate::config::prompt_cache_key::PromptCacheKeySettings,
    #[serde(default, skip_serializing_if = "crate::config::health::HealthSettings::is_default")]
    pub health: crate::config::health::HealthSettings,
    #[serde(default, skip_serializing_if = "crate::short_ids::Settings::is_default")]
    pub agent_short_ids: crate::short_ids::Settings,
    /// Artifact store limits and project auto capture (`artifacts`). r222
    /// and older ignore and drop it.
    #[serde(default, skip_serializing_if = "crate::config::artifacts::ArtifactSettings::is_default")]
    pub artifacts: crate::config::artifacts::ArtifactSettings,
    #[serde(default, skip_serializing_if = "crate::config::browser::BrowserSettings::is_default")]
    pub browser: crate::config::browser::BrowserSettings,
    /// Workflow limits and approval (`workflows`, cutex `workflows`). Older
    /// cutex ignores and drops it.
    #[serde(default, skip_serializing_if = "crate::workflows::WorkflowSettings::is_default")]
    pub workflows: crate::workflows::WorkflowSettings,
}

impl Default for CodezConfig {
    fn default() -> Self {
        Self {
            asks: None,
            appearance: Default::default(),
            docker_use_sudo: false,
            custom_status_items: Vec::new(),
            proxy: None,
            claude_cli: Default::default(),
            session: default_session_config(),
            claude_usage_refresh_minutes: default_claude_usage_refresh_minutes(),
            profile_quota_refresh_minutes: default_profile_quota_refresh_minutes(),
            archive_delete_empty: default_archive_delete_empty(),
            queue: QueueSettings::default(),
            image_input: Default::default(),
            auto_thread_titles: true,
            codex_config_follow_shared: true,
            thread_title_model: None,
            default_profile: None,
            default_profile_direct_launch: false,
            quick_chat_default_profile: None,
            new_session_defaults: Default::default(),
            new_thread_defaults: Default::default(),
            new_session_notification: Default::default(),
            terminal_truecolor: false,
            default_local_host_filter: false,
            windows_service_controller: None,
            agent_sort: Default::default(),
            session_sort: Default::default(),
            project_sort: Default::default(),
            job_sort: Default::default(),

            notify_service_url: None,
            notify_service_token: None,
            notify_service_idle_timeout_secs: None,
            notify_service_composer_idle_timeout_secs: None,
            notify_service_approval_timeout_secs: None,
            notify_service_startup_idle_timeout_secs: None,
            notify_service_events: None,
            notify_service_user_message_content: None,
            notify_service_user_message_preview_chars: None,
            rate_limit_threshold_warning_mode: None,
            rate_limit_model_nudge_mode: None,
            desktop_notify_enabled: false,
            desktop_notify_port: None,
            desktop_notify_token: None,
            peers: Default::default(),
            agent_bus_enabled: true,
            agent_bus_port: None,
            agent_bus_token: None,
            agent_bus_audit: Default::default(),
            agent_bus: Default::default(),
            instance: None,
            management_port: None,
            private_job_presentation: None,
            management_api_token: None,
            agent_security: Default::default(),
            attachments: Default::default(),
            documents: Default::default(),
            usage: Default::default(),
            management_listen: None,
            management_cors_origins: None,
            owner_notify: Default::default(),
            notification_levels: Default::default(),
            session_store: Default::default(),
            agent_harness: Default::default(),
            prompt_cache_key: Default::default(),
            health: Default::default(),
            agent_short_ids: Default::default(),
            artifacts: Default::default(),
            workflows: Default::default(),
            browser: Default::default(),
        }
    }
}



fn default_claude_usage_refresh_minutes() -> u8 {
    5
}

fn default_archive_delete_empty() -> bool {
    true
}

fn default_profile_quota_refresh_minutes() -> u8 {
    5
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub no_proxy: Option<String>,
    // `force_http_transport` (retired, never read by any native) is ignored on read and dropped on
    // the next save.
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CustomStatusItemsCatalogFile {
    #[serde(default)]
    pub items: Vec<CustomStatusItemCatalogEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CustomStatusItemCatalogEntry {
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub animation: Option<serde_json::Value>,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source: CustomStatusItemSource,
    #[serde(default)]
    pub render: CustomStatusItemRender,
    #[serde(default)]
    pub style: CustomStatusItemStyle,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomStatusItemSource {
    Static {
        value: String,
    },
    Env {
        key: String,
        #[serde(default = "default_true")]
        trim: bool,
    },
    FileText {
        path: String,
        #[serde(default = "default_true")]
        trim: bool,
    },
    LaunchProfile,
    LaunchRuntime,
    LaunchProfileSource,
    LaunchProfileType,
    LaunchProfileEmail,
    CurrentDir,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomStatusItemRender {
    Value,
    LabelValue { label: String },
    Template { template: String },
}

impl Default for CustomStatusItemRender {
    fn default() -> Self {
        Self::Value
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct CustomStatusItemStyle {
    #[serde(default)]
    pub fg: Option<String>,
    #[serde(default)]
    pub bg: Option<String>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underlined: bool,
}

fn default_true() -> bool {
    true
}

fn default_session_config() -> SessionConfig {
    SessionConfig { enabled: false }
}

#[derive(Debug)]
pub struct ImportedSnapshot {
    pub raw_auth_json: String,
    pub raw_config_toml: Option<String>,
    pub raw_model_catalog_json: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct MaterializedAccountFiles {
    pub auth_path: PathBuf,
    pub config_path: PathBuf,
    pub model_catalog_path: PathBuf,
    pub custom_status_items_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::quota::Preset;

    // Frozen 8912813 pre-browser account/options parsers, NOT current types.
    // Older StoredAccount readers ignore unknown top-level fields but recurse
    // into this strict options object. This is the rollback compatibility edge.
    #[derive(Default, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    #[allow(dead_code)]
    struct PreBrowserOptions {
        quota: crate::profiles::quota::Config,
        quotas: Vec<crate::profiles::quota::Config>,
        provider: Option<crate::profiles::appearance::Provider>,
        style: Option<crate::profiles::appearance::TextStyle>,
        claude: crate::profiles::appearance::ClaudeDefaults,
        allow_session_switch: Option<bool>,
        native_subagents: Option<bool>,
        delegation_wait: Option<bool>,
        agent_use_requires_approval: bool,
        agent_turn_questions: Option<crate::profiles::appearance::AgentTurnQuestions>,
        claude_file_checkpointing: Option<bool>,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct PreBrowserAccount {
        #[serde(default)]
        options: PreBrowserOptions,
        id: String,
        name: String,
        email: Option<String>,
        plan_type: Option<String>,
        #[serde(default)]
        plan_label: Option<String>,
        #[serde(default)]
        quota_preset: Preset,
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
        last_used_at: Option<DateTime<Utc>>,
        #[serde(default)]
        provider_category: Option<String>,
        #[serde(default)]
        claude_account: Option<String>,
        #[serde(default)]
        model_display_names: std::collections::BTreeMap<String, String>,
    }

    #[derive(Deserialize)]
    struct PreBrowserAccountsStore {
        version: u32,
        accounts: Vec<PreBrowserAccount>,
        active_account_id: Option<String>,
    }

    #[test]
    fn dsh_profile_defaults_are_omitted_and_nondefaults_roundtrip() {
        for kind in ["codex", "claude", "dsh"] {
            let mut account: StoredAccount = serde_json::from_value(serde_json::json!({
                "id":"dsh-default-compat", "name":"compat", "cli_kind":kind
            })).unwrap();
            let saved = serde_json::to_value(&account).unwrap();
            assert!(saved["options"].get("dsh").is_none());
            let restored: StoredAccount = serde_json::from_value(saved).unwrap();
            assert_eq!(restored.options.dsh, super::super::dsh::DshOptions::default());
            account.options.dsh.model = "deepseek-custom".into();
            account.options.dsh.reasoning_effort = Some("max".into());
            account.options.dsh.api_key_env = "MY_DEEPSEEK_KEY".into();
            let saved = serde_json::to_value(&account).unwrap();
            assert_eq!(saved["options"]["dsh"], serde_json::json!({
                "model":"deepseek-custom", "reasoningEffort":"max", "apiKeyEnv":"MY_DEEPSEEK_KEY"
            }));
            let restored: StoredAccount = serde_json::from_value(saved).unwrap();
            assert_eq!(restored.options.dsh, account.options.dsh);
        }
    }

    #[test]
    fn browser_opt_in_store_is_readable_by_pre_browser_accounts_parser() {
        for kind in ["codex", "claude"] {
            let mut account: StoredAccount = serde_json::from_value(serde_json::json!({
                "id": "fixture-profile", "name": "compat", "cli_kind": kind
            })).unwrap();
            assert!(!account.browser_tools, "old accounts default off");
            assert!(serde_json::to_value(&account).unwrap().get("browser_tools").is_none());
            account.browser_tools = true;
            let store = AccountsStore {
                version: STORE_VERSION,
                accounts: vec![account],
                active_account_id: Some("fixture-profile".into()),
            };
            let saved = serde_json::to_value(store).unwrap();
            assert_eq!(saved["accounts"][0]["browser_tools"], true);
            assert!(saved["accounts"][0]["options"].get("browser_tools").is_none());
            let legacy: PreBrowserAccountsStore = serde_json::from_value(saved.clone()).unwrap();
            assert_eq!(legacy.version, STORE_VERSION);
            assert_eq!(legacy.accounts[0].name, "compat");
            assert_eq!(legacy.active_account_id.as_deref(), Some("fixture-profile"));
            assert_eq!(legacy.accounts[0].cli_kind.to_string(), kind);
            let current: AccountsStore = serde_json::from_value(saved.clone()).unwrap();
            assert!(current.accounts[0].browser_tools);

            let mut unpublished_experiment = saved;
            unpublished_experiment["accounts"][0]["options"]["browser_tools"] = true.into();
            assert!(serde_json::from_value::<PreBrowserAccountsStore>(unpublished_experiment.clone()).is_err());
            // Persistence tolerates unknown fields (backend auto-update RFC
            // §6.2): the nested key is neither migrated nor dropped on save.
            let tolerant = serde_json::from_value::<AccountsStore>(unpublished_experiment).unwrap();
            assert!(tolerant.accounts[0].browser_tools, "the top-level opt-in still decides");
            let resaved = serde_json::to_value(&tolerant).unwrap();
            assert_eq!(resaved["accounts"][0]["options"]["browser_tools"], true, "kept for the writer that knows it");
        }
    }

    #[test]
    fn profile_options_from_a_newer_cutex_load_and_keep_their_fields() {
        let newer = serde_json::json!({
            "version": STORE_VERSION,
            "active_account_id": null,
            "accounts": [{
                "id": "fixture-profile", "name": "newer", "cli_kind": "codex",
                "options": {
                    "future_option": {"x": 1},
                    "quota": {"enabled": false, "future_quota_field": "y"},
                    "claude": {"model": "sonnet", "future_claude_field": true},
                    "style": {"fg": "#123456", "future_style_field": 2}
                }
            }]
        });
        let store: AccountsStore = serde_json::from_value(newer).unwrap();
        let options = &store.accounts[0].options;
        assert_eq!(options.extra["future_option"], serde_json::json!({"x": 1}));
        let saved = serde_json::to_value(&store).unwrap();
        let options = &saved["accounts"][0]["options"];
        assert_eq!(options["future_option"]["x"], 1);
        assert_eq!(options["quota"]["future_quota_field"], "y");
        assert_eq!(options["claude"]["future_claude_field"], true);
        assert_eq!(options["style"]["future_style_field"], 2);
        // Nothing extra appears for an ordinary store.
        let plain = serde_json::to_value(ProfileOptionsProbe::default().0).unwrap();
        assert!(plain.get("extra").is_none(), "{plain}");
    }

    #[derive(Default)]
    struct ProfileOptionsProbe(crate::profiles::appearance::ProfileOptions);

    #[test]
    fn profile_additive_fields_stay_top_level_for_older_readers() {
        let account = StoredAccount {
            options: Default::default(),
            browser_tools: false,
            search_tool: false,
            quota_reader: None,
            private_chatgpt_auth_file: false,
            auto_pick: false,
            claude_code: Default::default(),
            variants: Default::default(),
            id: "0b2f6987-6b65-4aac-8e0f-e14b9e0a4d63".to_string(),
            name: "compat".to_string(),
            email: None,
            plan_type: Some("pro".to_string()),
            plan_label: Some("GLM Pro".to_string()),
            quota_preset: Preset::Auto,
            source: Some("official".to_string()),
            runtime: RuntimeConfig::Host,
            proxy: None,
            session: None,
            cli_kind: CliKind::Codex,
            default_cli_args: Vec::new(),
            agent_name: None,
            last_used_at: None,
            provider_category: None,
            claude_account: None,
            model_display_names: Default::default(),
            interface_base_urls: Default::default(),
        };

        let serialized = serde_json::to_value(&account).unwrap();
        assert_eq!(serialized["plan_label"], "GLM Pro");
        assert_eq!(serialized["quota_preset"], "auto");
        let options = serialized["options"].as_object().unwrap();
        assert!(!options.contains_key("plan_label"));
        assert!(!options.contains_key("quota_preset"));
        let quota = serialized["options"]["quota"].as_object().unwrap();
        assert!(!quota.contains_key("preset"));
        assert!(!quota.contains_key("balance"));

        let mut old_shape = serialized.clone();
        old_shape.as_object_mut().unwrap().remove("plan_label");
        old_shape.as_object_mut().unwrap().remove("quota_preset");
        // StoredAccount intentionally allows unknown top-level fields. An
        // older reader can likewise ignore these additive fields.
        old_shape["future_top_level_field"] = serde_json::json!("ignored");
        let old_account: StoredAccount = serde_json::from_value(old_shape).unwrap();
        assert_eq!(old_account.plan_label, None);
        assert_eq!(old_account.quota_preset, Preset::Custom);
        assert_eq!(old_account.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn config_with_r218_task_service_keys_still_loads() {
        use crate::profiles::list_preferences::ListSort;
        // r219 removed the Task Service; config.json written by r218 keeps these
        // keys until the next save. CodezConfig has no deny_unknown_fields.
        let old = serde_json::json!({
            "task_watchdog": {"poll_secs": 30, "stale_secs": 600, "escalation_secs": 900},
            "task_sort": "name_desc",
            "owner_task_read_credentials": [{
                "principal_id": "owner-reader",
                "audience": "example-backend",
                "token": "owner-reader-token-0123456789",
                "project_ids": ["project-alpha"],
                "expires_at": "2099-01-01T00:00:00Z"
            }],
            "project_sort": "name_asc",
            "job_sort": "name_desc",
        });
        let config: CodezConfig = serde_json::from_value(old).unwrap();
        assert_eq!((config.project_sort, config.job_sort), (ListSort::NameAsc, ListSort::NameDesc));
        let saved = serde_json::to_value(&config).unwrap();
        assert!(saved.get("task_sort").is_none(), "{saved}");
        assert!(saved.get("task_watchdog").is_none(), "{saved}");
        assert!(saved.get("owner_task_read_credentials").is_none(), "{saved}");
        assert!(!saved.to_string().contains("owner-reader-token"));
    }
}
