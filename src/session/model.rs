//! Durable cutex session value types.

use serde::Deserialize;
use serde::Serialize;

use std::collections::HashMap;

use chrono::Utc;

use crate::agent_bus::model::AgentRegistrationClass;
use crate::platform::host::current_host_name;
use crate::session::identity::default_cutex_session_id_for_codex_session;
use crate::session::identity::normalize_codex_session_id;
use crate::session::identity::normalize_cutex_session_id;

/// Largest revision that can be represented exactly by every JSON consumer.
pub const MAX_DURABLE_SESSION_REVISION: u64 = 9_007_199_254_740_991;

fn default_durable_session_revision() -> u64 {
    1
}

fn runtime_history_unknown(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CutexSessionArchiveState {
    #[default]
    Active,
    Retired,
}

impl CutexSessionArchiveState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CutexSessionQuickActionMode {
    #[default]
    Auto,
    Pinned,
    Hidden,
}

impl CutexSessionQuickActionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pinned => "pinned",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutexSessionUserAction {
    Attach,
    Takeover,
    Online,
    ResumeAttach,
    ResumeHere,
    ResumeManaged,
    KillAndResume,
}

impl CutexSessionUserAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::Takeover => "takeover",
            Self::Online => "online",
            Self::ResumeAttach => "resume-attach",
            Self::ResumeHere => "resume-here",
            Self::ResumeManaged => "resume-managed",
            Self::KillAndResume => "kill-and-resume",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CutexSessionRuntimeBackend {
    #[default]
    Host,
    HostForeground,
    Docker,
    CuteAlden,
    ClaudeSubscription,
    Dsh,
    /// r266: Kimi Code (`kimi acp`) through cutex's ACP adapter; the record's
    /// `dsh_session` (flavor `kimi`) holds its binding.
    Kimi,
    Future,
}

impl CutexSessionRuntimeBackend {
    /// The runtimes driven through cutex's ACP adapter protocol (DSH, Kimi Code).
    pub fn is_acp(self) -> bool {
        matches!(self, Self::Dsh | Self::Kimi)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutexAppServerTransport {
    UnixSocket,
    LoopbackWebSocket,
}

/// Immutable provenance for the profile selected for one managed runtime
/// occurrence.  This is deliberately distinct from `CutexSessionRecord::profile`,
/// which is durable configured intent for future launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchProfileSource {
    OneLaunchOverride,
    SessionConfigured,
    GlobalDefault,
    Unknown,
}

impl LaunchProfileSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneLaunchOverride => "one_launch_override",
            Self::SessionConfigured => "session_configured",
            Self::GlobalDefault => "global_default",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutexAppServerRuntimeBinding {
    pub transport: CutexAppServerTransport,
    pub endpoint: String,
    pub pid: u32,
    pub runtime_dir: String,
    /// Profile actually used to launch this app-server occurrence. `None` is
    /// retained for legacy bindings whose launch evidence predates this field;
    /// it must never be inferred from the current global default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_profile: Option<String>,
    /// Stable id of the launched profile (r218+). The id is the identity: a
    /// rename rewrites `launched_profile` and readers resolve the current
    /// name from this id. Absent on bindings written by r217 and older.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_profile_id: Option<String>,
    /// Source captured at occurrence creation. A missing value is legacy
    /// evidence and projects as `unknown`; it is never inferred later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_profile_source: Option<LaunchProfileSource>,
    #[serde(default)]
    pub auth_token_path: Option<String>,
    pub diagnostic_journal_path: String,
    pub schema_version: String,
    pub schema_sha256: String,
    pub started_at: String,
}

impl CutexAppServerRuntimeBinding {
    /// The launched profile under its current name: resolved by
    /// `launched_profile_id`, else (r217-launched binding) the recorded name
    /// traced through the rename history at `started_at`. A rename never
    /// rewrites the binding (it is occurrence evidence compared byte for
    /// byte with launch receipts), so read the profile through this.
    pub fn current_launched_profile(&self) -> Option<String> {
        self.current_launched_profile_ref().name
    }

    /// `(id, current name)` of the launched profile (see
    /// [`Self::current_launched_profile`]).
    pub fn current_launched_profile_ref(&self) -> crate::profiles::identity::LaunchedProfile {
        crate::profiles::identity::LaunchedProfile::resolve_at(
            self.launched_profile_id.as_deref(),
            self.launched_profile.as_deref(),
            Some(self.started_at.as_str()),
        )
    }
}

pub fn normalize_runtime_token(value: &str) -> String {
    value.trim().replace(['_', ' '], "-").to_ascii_lowercase()
}

pub fn parse_cutex_session_runtime_backend(
    value: &str,
) -> anyhow::Result<CutexSessionRuntimeBackend> {
    match normalize_runtime_token(value).as_str() {
        "claude_subscription" | "claude-subscription" => Ok(CutexSessionRuntimeBackend::ClaudeSubscription),
        "host" | "local" => Ok(CutexSessionRuntimeBackend::Host),
        "host-foreground" | "host_foreground" | "foreground" | "native" | "windows-native"
        | "windows_native" | "host-fg" | "host_fg" => {
            Ok(CutexSessionRuntimeBackend::HostForeground)
        }
        "docker" => Ok(CutexSessionRuntimeBackend::Docker),
        "cute-alden" | "alden" | "background" | "attachable" => {
            Ok(CutexSessionRuntimeBackend::CuteAlden)
        }
        "claude-subscription" => Ok(CutexSessionRuntimeBackend::ClaudeSubscription),
        "future" => Ok(CutexSessionRuntimeBackend::Future),
        "dsh" => Ok(CutexSessionRuntimeBackend::Dsh),
        "kimi" => Ok(CutexSessionRuntimeBackend::Kimi),
        other => anyhow::bail!("unsupported runtime_backend: {other}"),
    }
}

pub fn parse_cutex_session_quick_action_mode(
    value: &str,
) -> anyhow::Result<CutexSessionQuickActionMode> {
    match normalize_runtime_token(value).as_str() {
        "auto" | "automatic" => Ok(CutexSessionQuickActionMode::Auto),
        "pin" | "pinned" => Ok(CutexSessionQuickActionMode::Pinned),
        "hide" | "hidden" => Ok(CutexSessionQuickActionMode::Hidden),
        other => anyhow::bail!("unsupported quick action mode: {other}"),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CutexSessionStore {
    /// Latest proven stopped native occurrence, saved with runtime clearing.
    #[serde(default)]
    pub native_stop_receipts: std::collections::BTreeMap<String, NativeStopReceipt>,
    #[serde(default)]
    pub explicit_launch_receipts:
        std::collections::BTreeMap<String, crate::agent_management::ExplicitLaunchActionReceipt>,
    #[serde(default)]
    pub human_adoption_receipts:
        std::collections::BTreeMap<String, crate::agent_management::HumanAdoptReceipt>,
    /// Archive transition receipt committed atomically with its durable state.
    #[serde(default)]
    pub agent_archive_receipts:
        std::collections::BTreeMap<String, crate::agent_management::AgentArchiveReceipt>,
    /// Exact receipts for Human import naming; committed atomically with the name.
    #[serde(default)]
    pub formal_name_receipts: std::collections::BTreeMap<String, FormalAgentNameReceipt>,
    /// Explicit-launch actions removed by receipt retention
    /// (`crate::session::receipt_retention`): a replay or status query of one
    /// of these keys fails with `explicit_launch_action_pruned` instead of
    /// starting a new action. Older cutex ignores this map (and drops it on
    /// its next save).
    #[serde(
        default,
        rename = "explicitLaunchTombstones",
        skip_serializing_if = "std::collections::BTreeMap::is_empty"
    )]
    pub explicit_launch_tombstones: std::collections::BTreeMap<
        String,
        crate::session::receipt_retention::ExplicitLaunchTombstone,
    >,
    /// Internal compare-and-swap generation for cross-process writers.
    #[serde(default, rename = "storeRevision")]
    pub store_revision: StoreRevisionCell,
    #[serde(default)]
    pub sessions: HashMap<String, CutexSessionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeStopReceipt {
    pub native_id: Option<String>,
    pub generation: u64,
    pub binding: Option<CutexAppServerRuntimeBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FormalAgentNameReceipt {
    pub request_sha256: crate::role_revision::Sha256,
    pub cutex_session_id: String,
    pub name: String,
    pub previous_revision: u64,
    pub revision: u64,
    pub previous_updated_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CutexSessionAvatarKind {
    Emoji,
    Color,
    Icon,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CutexSessionAvatar {
    pub kind: CutexSessionAvatarKind,
    pub value: String,
    /// Optional circle colour `#rrggbb` (lowercase) for an `emoji` or `icon`
    /// avatar (capability `agent-avatar-color`). A `color` avatar carries its
    /// colour in `value` and never has this field. Pre-r228 readers ignore it
    /// (no `deny_unknown_fields`) and drop it on their next save of the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// `#rrggbb` (either case): the only accepted `avatar.color` form.
pub fn is_avatar_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit)
}

/// Immutable actor observed by the authorized creation path, never inferred
/// from today's project authority or from an imported/restored session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum SessionCreatedBy {
    Owner,
    System,
    Agent {
        #[serde(rename = "cutexSessionId")]
        cutex_session_id: crate::role_revision::CutexSessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<SessionCreatorRole>,
    },
}

impl<'de> Deserialize<'de> for SessionCreatedBy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Empty struct variants reject extra keys; serde unit variants ignore them.
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
        enum Wire {
            Owner {},
            System {},
            Agent {
                #[serde(rename = "cutexSessionId")]
                cutex_session_id: crate::role_revision::CutexSessionId,
                #[serde(default)]
                role: Option<SessionCreatorRole>,
            },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Owner {} => Self::Owner,
            Wire::System {} => Self::System,
            Wire::Agent { cutex_session_id, role } => Self::Agent { cutex_session_id, role },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionCreatorRole {
    Director,
    Member,
}

/// Explicit presentation intent. None/missing is legacy fixed assignment, not
/// dynamic follow. Stored fixed avatars are never rewritten by Profile styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CutexSessionAvatarMode {
    Fixed,
    Inherit,
}

/// Durable revisions at which the presentation metadata last changed. Every
/// change at a revision `>= since` is recorded; a field left `None` has not
/// changed since `since`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CutexSessionMetadataChanges {
    pub since: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Shared CAS domain for the avatar value AND avatar mode.
    pub avatar: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<u64>,
}

impl CutexSessionMetadataChanges {
    pub fn since(revision: u64) -> Self {
        Self { since: revision, ..Self::default() }
    }
}

/// Presentation-only badge with a bounded display width.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CutexSessionBadge {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tint: Option<String>,
}

pub fn validate_session_badge(badge: &CutexSessionBadge) -> Result<(), &'static str> {
    if !valid_badge_text(&badge.text) {
        return Err("badge.text must fit 1..4 display cells, 8 characters and 32 bytes, without whitespace, controls or invisible formatting");
    }
    if badge.tint.as_deref().is_some_and(|t| !["blue", "green", "amber", "pink", "violet"].contains(&t)) {
        return Err("badge.tint must be blue, green, amber, pink or violet");
    }
    Ok(())
}

/// Shared session/project badge limits. Measure the whole string so combining
/// scripts, emoji presentation selectors and emoji ZWJ sequences keep their
/// rendered width (unicode-width), rather than summing individual scalar widths.
/// Unicode 17 Cf/default-ignorable controls stay forbidden except contextual
/// joiners/variation selectors needed to shape visible text.
pub fn valid_badge_text(text: &str) -> bool {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    if text.len() > 32 || text.chars().count() > 8
        || !(1..=4).contains(&UnicodeWidthStr::width(text))
    {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut has_base = false;
    for (index, &c) in chars.iter().enumerate() {
        let joiner = matches!(c, '\u{200c}' | '\u{200d}');
        let selector = matches!(c as u32,
            0x180B..=0x180D | 0x180F | 0xFE00..=0xFE0F | 0xE0100..=0xE01EF);
        if c.is_control() || c.is_whitespace()
            || (!joiner && !selector && matches!(c as u32,
                0x00AD | 0x034F | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F
                | 0x0890..=0x0891 | 0x08E2 | 0x115F..=0x1160 | 0x17B4..=0x17B5
                | 0x180B..=0x180F | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F
                | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFFB
                | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A | 0xE0000..=0xE0FFF))
        {
            return false;
        }
        let visible = UnicodeWidthChar::width(c).is_some_and(|width| width > 0);
        // A mark/selector cannot make an invisible-only or leading-mark badge.
        if !visible && !has_base {
            return false;
        }
        if selector && index > 0 && matches!(chars[index - 1], '\u{200c}' | '\u{200d}') {
            return false;
        }
        // Joiners must connect visible content, never trail a badge or form an
        // invisible chain. Script combining marks may occur before the next base.
        if joiner && !chars[index + 1..].iter()
            .take_while(|&&next| !matches!(next, '\u{200c}' | '\u{200d}'))
            .any(|&next| UnicodeWidthChar::width(next).is_some_and(|width| width > 0))
        {
            return false;
        }
        has_base |= visible;
    }
    has_base
}

/// Shared avatar contract check (Management validation and the store patch).
pub fn validate_session_avatar(avatar: &CutexSessionAvatar) -> Result<(), &'static str> {
    if avatar.value.trim().is_empty()
        || avatar.value.chars().count() > 64
        || avatar.value.chars().any(char::is_control)
    {
        return Err("avatar.value must be a non-empty string of at most 64 characters");
    }
    match (&avatar.kind, &avatar.color) {
        (_, None) => Ok(()),
        (CutexSessionAvatarKind::Color, Some(_)) => {
            Err("avatar.color is only for emoji / icon avatars (a color avatar keeps its colour in value)")
        }
        (_, Some(color)) if is_avatar_hex_color(color) => Ok(()),
        (_, Some(_)) => Err("avatar.color must be #rrggbb"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CutexSessionRecord {
    /// Execution selection, never identity. Survives all runtime clearing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit_launch: Option<crate::agent_management::ExplicitLaunchContract>,
    /// Claude identity is not a Codex rollout ID or a selected OpenAI profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session: Option<crate::runtime::claude::ClaudeSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dsh_session: Option<crate::runtime::dsh::DshSession>,
    /// r265: profile + runtime variant + credential reference frozen at
    /// creation (`profiles::runtime_variants`). Absent on older sessions,
    /// which stay bound to their profile's primary runtime. No secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_binding: Option<crate::profiles::runtime_variants::ProfileLaunchBinding>,
    pub cutex_session_id: String,
    /// Durable specification/lifecycle revision.  Runtime occurrence changes
    /// are fenced separately by `runtime_generation`.
    #[serde(
        default = "default_durable_session_revision",
        alias = "durable_revision",
        alias = "durableRevision"
    )]
    pub revision: u64,
    #[serde(default, rename = "lifecycle", alias = "archive_state")]
    pub archive_state: CutexSessionArchiveState,
    #[serde(default, rename = "retiredAt", alias = "retired_at")]
    pub retired_at: Option<String>,
    #[serde(default)]
    pub codex_session_id: Option<String>,
    #[serde(default)]
    // Native launch correlation reported by historical heartbeat flows.
    pub pending_launch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // Crash fence owned only by a Management app-server launch attempt.
    pub app_server_launch_claim_id: Option<String>,
    #[serde(default)]
    pub thread_name: Option<String>,
    #[serde(default)]
    pub display_name_hint: Option<String>,
    /// Explicit Cutex Agent name. Never inferred from native thread metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formal_agent_name: Option<String>,
    /// Human-chosen display label (Management `cutex/session/metadata/update`).
    /// Presentation only: never an identity, bus address, or authority name;
    /// `formal_agent_name` stays authoritative for those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name_override: Option<String>,
    /// Human note shown by clients. Presentation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Human-chosen avatar. Presentation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<CutexSessionAvatar>,
    /// Explicit dynamic follow; None preserves the legacy fixed behavior.
    /// Inherit stores no avatar and automatic avatar-only assignments refuse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_mode: Option<CutexSessionAvatarMode>,
    /// Creation-time provenance. Missing/null means unknown, including legacy
    /// sessions and imports; it must never be backfilled from current roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<SessionCreatedBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<CutexSessionBadge>,
    /// When `display_name_override` / `note` / `avatar` last changed (r252),
    /// so a fenced `cutex/session/metadata/update` is refused only when a
    /// field it writes changed after the client's revision, not for every
    /// unrelated (often internal) write that moved the session revision.
    /// Absent (records from older cutex, which drop it on save): unknown,
    /// and the fence stays an exact revision match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_changes: Option<CutexSessionMetadataChanges>,
    pub host_id: String,
    pub cwd: String,
    #[serde(default)]
    pub managed_cwd: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    /// Last verified Codex conversation family. Clearing its profile must not
    /// erase the compatibility boundary and permit a cross-provider rebind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_family: Option<String>,
    #[serde(default)]
    pub runtime_backend: CutexSessionRuntimeBackend,
    #[serde(default)]
    pub agent_enabled: bool,
    #[serde(default)]
    pub agent_groups: Vec<String>,
    #[serde(default)]
    pub registration_class: AgentRegistrationClass,
    #[serde(default)]
    pub exposed_to_backend: bool,
    #[serde(default)]
    pub quick_action: CutexSessionQuickActionMode,
    #[serde(default)]
    pub default_cli_args: Vec<String>,
    #[serde(default)]
    pub permission_defaults: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<String>,
    /// Native `approvals_reviewer` (`user` | `auto_review`) chosen at creation
    /// and mirrored from `thread/settings/updated`; sent on every thread start /
    /// resume. None follows the profile config / native default (`user`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<String>,
    /// workspace-write only: allow outbound network from sandboxed commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_network_access: Option<bool>,
    /// workspace-write only: extra writable roots besides the thread cwd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandbox_writable_roots: Vec<String>,
    #[serde(default)]
    pub model_defaults: Option<String>,
    #[serde(default)]
    pub reasoning_defaults: Option<String>,
    /// Native service tier for this session (`session-fast-mode`): `priority`
    /// (Fast), `default` (standard) or `flex`; None follows the profile config.
    /// Mirrored from `thread/settings/updated` and sent on every thread start /
    /// resume, because the native keeps a thread's tier only while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub alden_session_name: Option<String>,
    #[serde(default)]
    pub alden_pid: Option<u32>,
    #[serde(default)]
    pub runtime_pid: Option<u32>,
    #[serde(default)]
    pub app_server_runtime: Option<CutexAppServerRuntimeBinding>,
    #[serde(default)]
    pub current_runtime_agent_id: Option<String>,
    #[serde(default)]
    pub runtime_generation: u64,
    /// Generation zero is evidence of no managed launch only for records
    /// created with occurrence tracking; legacy default-zero is unknown.
    #[serde(default, skip_serializing_if = "runtime_history_unknown")]
    pub runtime_history_known: bool,
    #[serde(default)]
    pub last_runtime_agent_id: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub last_user_selected_at: Option<String>,
    #[serde(default)]
    pub last_user_action: Option<CutexSessionUserAction>,
    pub created_at: String,
    pub updated_at: String,
}

impl CutexSessionRecord {
    /// Creation-only seed, before this new record's FIRST durable publication.
    /// A None seed leaves unknown provenance unchanged; retries may only repeat
    /// the exact captured actor. Do not call this to enrich a restored/imported
    /// identity, or pass a source session's creator when making a new fork.
    pub fn stamp_created_by_at_creation(&mut self, creator: Option<&SessionCreatedBy>) -> anyhow::Result<()> {
        let Some(creator) = creator else { return Ok(()) };
        if let Some(existing) = &self.created_by {
            anyhow::ensure!(existing == creator, "session creator is immutable");
        } else {
            self.created_by = Some(creator.clone());
        }
        Ok(())
    }

    /// The app-server conversation ID is distinct from the backend's native history ID.
    pub fn app_server_thread_id(&self) -> Option<&str> {
        self.claude_session.as_ref().map(|session| session.thread_id.as_str())
            .or_else(|| self.dsh_session.as_ref().map(|session| session.thread_id.as_str()))
            .or(self.codex_session_id.as_deref())
    }
    /// Runtime ownership does not grant managed Agent identity or Project authority.
    pub fn is_owned_session(&self) -> bool {
        (self.explicit_launch.is_some() || matches!(self.runtime_backend, CutexSessionRuntimeBackend::ClaudeSubscription | CutexSessionRuntimeBackend::Dsh | CutexSessionRuntimeBackend::Kimi)) && !self.agent_enabled
            && self.registration_class == crate::agent_bus::model::AgentRegistrationClass::LocalOnly
            && self.formal_agent_name.is_none()
    }

    #[allow(dead_code)]
    pub fn new(
        cutex_session_id: String,
        codex_session_id: Option<String>,
        host_id: String,
        cwd: String,
        profile: Option<String>,
    ) -> anyhow::Result<Self> {
        let now = Utc::now().to_rfc3339();
        Self::new_at(
            cutex_session_id,
            codex_session_id,
            host_id,
            cwd,
            profile,
            now,
        )
    }

    pub fn new_at(
        cutex_session_id: String,
        codex_session_id: Option<String>,
        host_id: String,
        cwd: String,
        profile: Option<String>,
        timestamp: String,
    ) -> anyhow::Result<Self> {
        let cutex_session_id = normalize_cutex_session_id(&cutex_session_id)?;
        let codex_session_id = codex_session_id
            .map(|value| normalize_codex_session_id(&value))
            .transpose()?;
        Ok(Self {
            explicit_launch: None,
            claude_session: None,
            dsh_session: None,
            profile_binding: None,
            provider_family: None,
            cutex_session_id,
            revision: default_durable_session_revision(),
            archive_state: CutexSessionArchiveState::Active,
            retired_at: None,
            codex_session_id,
            pending_launch_id: None,
            app_server_launch_claim_id: None,
            thread_name: None,
            display_name_hint: None,
            formal_agent_name: None,
            display_name_override: None,
            note: None,
            avatar: None,
            avatar_mode: None,
            created_by: None,
            badge: None,
            metadata_changes: Some(CutexSessionMetadataChanges::since(
                default_durable_session_revision(),
            )),
            host_id,
            cwd: crate::session::metadata::normalize_session_cwd(&cwd),
            managed_cwd: None,
            profile,
            runtime_backend: CutexSessionRuntimeBackend::Host,
            agent_enabled: false,
            agent_groups: Vec::new(),
            registration_class: AgentRegistrationClass::LocalOnly,
            exposed_to_backend: false,
            quick_action: CutexSessionQuickActionMode::Auto,
            default_cli_args: Vec::new(),
            permission_defaults: None,
            approval_policy: None,
            approvals_reviewer: None,
            sandbox_mode: None,
            sandbox_network_access: None,
            sandbox_writable_roots: Vec::new(),
            model_defaults: None,
            reasoning_defaults: None,
            service_tier: None,
            alden_session_name: None,
            alden_pid: None,
            runtime_pid: None,
            app_server_runtime: None,
            current_runtime_agent_id: None,
            runtime_generation: 0,
            runtime_history_known: true,
            last_runtime_agent_id: None,
            last_seen_at: None,
            last_user_selected_at: None,
            last_user_action: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        })
    }

    #[allow(dead_code)]
    pub fn from_codex_session_id(codex_session_id: &str) -> anyhow::Result<Self> {
        let codex_session_id = normalize_codex_session_id(codex_session_id)?;
        let cutex_session_id = default_cutex_session_id_for_codex_session(&codex_session_id);
        let cwd = std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        Self::new(
            cutex_session_id,
            Some(codex_session_id),
            current_host_name(),
            cwd,
            None,
        )
    }

    pub fn is_retired(&self) -> bool {
        self.archive_state == CutexSessionArchiveState::Retired
    }

    pub fn is_active(&self) -> bool {
        !self.is_retired()
    }

    pub fn durable_revision(&self) -> u64 {
        if self.revision == 0 {
            default_durable_session_revision()
        } else {
            self.revision
        }
    }

    pub fn bump_durable_revision(&mut self) -> anyhow::Result<u64> {
        let current = self.durable_revision();
        self.revision = current
            .checked_add(1)
            .filter(|revision| *revision <= MAX_DURABLE_SESSION_REVISION)
            .ok_or_else(|| anyhow::anyhow!("cutex session durable revision exhausted"))?;
        Ok(self.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cutex_session_runtime_backend_accepts_common_aliases() {
        assert_eq!(
            parse_cutex_session_runtime_backend("host").expect("host should parse"),
            CutexSessionRuntimeBackend::Host
        );
        assert_eq!(
            parse_cutex_session_runtime_backend("host_foreground")
                .expect("host_foreground should parse"),
            CutexSessionRuntimeBackend::HostForeground
        );
        assert_eq!(
            parse_cutex_session_runtime_backend("native").expect("native should parse"),
            CutexSessionRuntimeBackend::HostForeground
        );
        assert_eq!(
            parse_cutex_session_runtime_backend("cute-alden").expect("cute-alden should parse"),
            CutexSessionRuntimeBackend::CuteAlden
        );
    }

    #[test]
    fn parse_cutex_session_quick_action_mode_accepts_common_aliases() {
        assert_eq!(
            parse_cutex_session_quick_action_mode("auto").expect("auto should parse"),
            CutexSessionQuickActionMode::Auto
        );
        assert_eq!(
            parse_cutex_session_quick_action_mode("pinned").expect("pinned should parse"),
            CutexSessionQuickActionMode::Pinned
        );
        assert_eq!(
            parse_cutex_session_quick_action_mode("hide").expect("hide should parse"),
            CutexSessionQuickActionMode::Hidden
        );
    }

    #[test]
    fn legacy_store_defaults_generation_and_writes_current_name() {
        let store: CutexSessionStore = serde_json::from_value(serde_json::json!({
            "sessions": {}
        }))
        .expect("deserialize legacy store");
        assert_eq!(store.store_revision.get(), 0);

        let current = serde_json::to_value(store).expect("serialize current store");
        assert_eq!(current["storeRevision"], 0);
        assert!(current.get("store_revision").is_none());
    }

    #[test]
    fn session_record_without_app_server_binding_still_deserializes() {
        let record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            None,
            "host-1".to_string(),
            "/tmp/worktree".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("create session record");
        let mut value = serde_json::to_value(record).expect("serialize session record");
        value
            .as_object_mut()
            .expect("session record should be an object")
            .remove("app_server_runtime");

        let restored: CutexSessionRecord =
            serde_json::from_value(value).expect("deserialize legacy session record");
        assert!(restored.app_server_runtime.is_none());
    }

    #[test]
    fn pre_archive_session_record_defaults_to_active_revision_one() {
        let record = CutexSessionRecord::new_at(
            "cutex-legacy".to_string(),
            None,
            "host-1".to_string(),
            "/tmp/worktree".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("create session record");
        let mut value = serde_json::to_value(record).expect("serialize session record");
        let object = value
            .as_object_mut()
            .expect("session record should be an object");
        object.remove("revision");
        object.remove("lifecycle");
        object.remove("retiredAt");

        let restored: CutexSessionRecord =
            serde_json::from_value(value).expect("deserialize legacy session record");

        assert_eq!(restored.durable_revision(), 1);
        assert_eq!(restored.archive_state, CutexSessionArchiveState::Active);
        assert_eq!(restored.retired_at, None);
    }

    #[test]
    fn archive_fields_read_legacy_aliases_and_write_exact_current_names() {
        let record = CutexSessionRecord::new_at(
            "cutex-archive-alias".to_string(),
            None,
            "host-1".to_string(),
            "/tmp/worktree".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("create session record");
        let mut value = serde_json::to_value(record).expect("serialize session record");
        let object = value
            .as_object_mut()
            .expect("session record should be an object");
        object.remove("revision");
        object.remove("lifecycle");
        object.remove("retiredAt");
        object.insert("durable_revision".to_string(), serde_json::json!(7));
        object.insert("archive_state".to_string(), serde_json::json!("retired"));
        object.insert(
            "retired_at".to_string(),
            serde_json::json!("2026-07-10T00:01:00Z"),
        );

        let restored: CutexSessionRecord =
            serde_json::from_value(value).expect("deserialize aliased session record");
        assert_eq!(restored.durable_revision(), 7);
        assert_eq!(restored.archive_state, CutexSessionArchiveState::Retired);
        assert_eq!(restored.retired_at.as_deref(), Some("2026-07-10T00:01:00Z"));

        let current = serde_json::to_value(restored).expect("serialize current session record");
        let current = current.as_object().expect("current record object");
        assert_eq!(current.get("revision"), Some(&serde_json::json!(7)));
        assert_eq!(
            current.get("lifecycle"),
            Some(&serde_json::json!("retired"))
        );
        assert_eq!(
            current.get("retiredAt"),
            Some(&serde_json::json!("2026-07-10T00:01:00Z"))
        );
        assert!(!current.contains_key("durable_revision"));
        assert!(!current.contains_key("archive_state"));
        assert!(!current.contains_key("retired_at"));
    }

    #[test]
    fn active_session_serializes_nullable_retired_at() {
        let record = CutexSessionRecord::new_at(
            "cutex-active".to_string(),
            None,
            "host-1".to_string(),
            "/tmp/worktree".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("create session record");
        let value = serde_json::to_value(record).expect("serialize session record");

        assert_eq!(value["revision"], 1);
        assert_eq!(value["lifecycle"], "active");
        assert!(value
            .get("retiredAt")
            .is_some_and(serde_json::Value::is_null));
    }

    #[test]
    fn legacy_pending_launch_id_does_not_materialize_an_app_server_claim() {
        let record = CutexSessionRecord::new_at(
            "cutex-1".to_string(),
            None,
            "host-1".to_string(),
            "/tmp/worktree".to_string(),
            None,
            "2026-07-10T00:00:00Z".to_string(),
        )
        .expect("create session record");
        let mut value = serde_json::to_value(record).expect("serialize session record");
        let object = value
            .as_object_mut()
            .expect("session record should be an object");
        object.remove("app_server_launch_claim_id");
        object.insert(
            "pending_launch_id".to_string(),
            serde_json::Value::String("legacy-heartbeat-launch".to_string()),
        );

        let restored: CutexSessionRecord =
            serde_json::from_value(value).expect("deserialize legacy session record");

        assert_eq!(
            restored.pending_launch_id.as_deref(),
            Some("legacy-heartbeat-launch")
        );
        assert!(restored.app_server_launch_claim_id.is_none());
    }

    #[test]
    fn legacy_runtime_binding_without_launched_profile_is_unknown() {
        let binding = serde_json::json!({
            "transport": "unix_socket",
            "endpoint": "unix:///tmp/runtime/app.sock",
            "pid": 4242,
            "runtime_dir": "/tmp/runtime",
            "diagnostic_journal_path": "/tmp/runtime/events.jsonl",
            "schema_version": "test",
            "schema_sha256": "hash",
            "started_at": "2026-07-10T00:00:00Z"
        });
        let restored: CutexAppServerRuntimeBinding =
            serde_json::from_value(binding).expect("legacy binding should deserialize");
        assert_eq!(restored.launched_profile, None);
        assert_eq!(restored.launch_profile_source, None);
        let serialized = serde_json::to_value(&restored).expect("legacy binding should serialize");
        assert!(serialized.get("launched_profile").is_none());
        assert!(serialized.get("launch_profile_source").is_none());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionReconcileOutcome {
    pub cutex_session_id: String,
    pub codex_session_id: String,
    pub events: Vec<CutexSessionReconcileEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutexSessionReconcileEvent {
    pub event_type: &'static str,
    pub summary: String,
    pub previous_runtime_agent_id: Option<String>,
    pub runtime_agent_id: Option<String>,
    pub previous_cutex_session_id: Option<String>,
}

/// The session store's compare-and-swap generation: a `Cell<u64>` that is
/// `Sync`, so a parsed store can be shared between threads (the read cache
/// in `session::store` hands out `Arc<CutexSessionStore>`). Serialized as a
/// plain number, exactly like the `Cell<u64>` it replaces.
#[derive(Debug, Default)]
pub struct StoreRevisionCell(std::sync::atomic::AtomicU64);

impl StoreRevisionCell {
    pub fn new(value: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(value))
    }

    pub fn get(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set(&self, value: u64) {
        self.0.store(value, std::sync::atomic::Ordering::Relaxed);
    }
}

thread_local! {
    static WHOLE_STORE_CLONES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Whole-store copies made on the calling thread so far. Every derived
/// `CutexSessionStore::clone` clones its `store_revision` exactly once, so
/// this counts them (diagnostics and tests that pin hot paths to zero
/// copies; a thread-local cell costs nothing next to a store copy).
pub fn whole_store_clones_on_this_thread() -> u64 {
    WHOLE_STORE_CLONES.with(std::cell::Cell::get)
}

impl Clone for StoreRevisionCell {
    fn clone(&self) -> Self {
        WHOLE_STORE_CLONES.with(|count| count.set(count.get() + 1));
        Self::new(self.get())
    }
}

impl Serialize for StoreRevisionCell {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.get())
    }
}

impl<'de> Deserialize<'de> for StoreRevisionCell {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        u64::deserialize(deserializer).map(Self::new)
    }
}

#[cfg(test)]
mod badge_text_tests {
    use super::*;

    #[test]
    fn badges_accept_emoji_presentation_joiners_and_combining_scripts() {
        for text in ["ABCD", "中文", "🙂", "❤️", "🏳️‍🌈", "👩🏽‍💻", "👨‍👩‍👧‍👦",
            "A\u{0301}", "a\u{0301}b\u{0301}c\u{0301}d\u{0301}", "क\u{093f}", "ن\u{064e}",
            "क\u{094d}\u{200c}ष", "漢\u{e0100}"]
        {
            assert!(validate_session_badge(&CutexSessionBadge { text: text.into(), tint: None }).is_ok(), "{text:?}");
        }
    }

    #[test]
    fn badges_reject_bidi_hidden_only_controls_and_unbounded_text() {
        for text in [format!("A{}", "\u{200b}".repeat(100_000)), "A\u{200d}".into(),
            "A\u{202e}".into(), "A\u{2066}".into(), "A\u{061c}".into(), "A\u{0600}".into(),
            "A\u{3164}".into(), "A\u{e0001}".into(), "A\u{034f}".into(), "\u{0301}".into(),
            "\u{200d}".into(), "\u{fe0f}".into(), "\u{200d}🙂".into(), "\u{0301}A".into(),
            "A\u{200d}\u{200d}B".into(), "A\u{200d}\u{fe0f}B".into(), "A B".into(), "A\n".into()]
        {
            assert!(!valid_badge_text(&text), "{text:?}");
        }
    }

    #[test]
    fn badge_width_is_measured_as_a_string_but_scalar_and_byte_caps_still_apply() {
        assert!(!valid_badge_text("ABCDE"));
        assert!(!valid_badge_text("中文A"));
        assert!(!valid_badge_text("❤️❤️❤️"));
        assert!(!valid_badge_text(&format!("A{}", "\u{0301}".repeat(8))));
        assert!(!valid_badge_text("👨‍👩‍👧‍👦A\u{0301}")); // nine scalars
        assert!(valid_badge_text("❤️❤️")); // four cells, four scalars
        assert!(valid_badge_text("👨‍👩‍👧‍👦A")); // eight scalars, three cells
    }
}

#[cfg(test)]
mod created_by_tests {
    use super::*;
    use serde_json::{json,Value};

    fn record()->CutexSessionRecord {
        CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000c258").unwrap()
    }
    fn director()->SessionCreatedBy {
        SessionCreatedBy::Agent {cutex_session_id:crate::role_revision::CutexSessionId::new("cutex.original-director").unwrap(),role:Some(SessionCreatorRole::Director)}
    }
    #[test]
    fn session_creator_old_missing_null_and_new_constructor_are_unknown() {
        let r=record();assert!(r.created_by.is_none());
        let mut stored=serde_json::to_value(&r).unwrap();assert!(stored.get("created_by").is_none());
        stored["created_by"]=Value::Null;
        let restored:CutexSessionRecord=serde_json::from_value(stored).unwrap();
        assert!(restored.created_by.is_none());
    }
    #[test]
    fn session_creator_wire_shapes_are_strict_and_capture_actual_id_and_role() {
        for creator in [SessionCreatedBy::Owner,SessionCreatedBy::System,director(),SessionCreatedBy::Agent {
            cutex_session_id:crate::role_revision::CutexSessionId::new("cutex.member").unwrap(),role:None,
        }] {
            let wire=serde_json::to_value(&creator).unwrap();
            let loaded:SessionCreatedBy=serde_json::from_value(wire.clone()).unwrap();assert_eq!(loaded,creator);
            if wire["kind"]=="agent" {assert!(wire["cutexSessionId"].as_str().is_some());}
            else {assert_eq!(wire.as_object().unwrap().len(),1);}
        }
        assert_eq!(serde_json::to_value(director()).unwrap(),json!({"kind":"agent","cutexSessionId":"cutex.original-director","role":"director"}));
        for bad in [json!({"kind":"owner","role":"director"}),json!({"kind":"system","cutexSessionId":"cutex.agent"}),
            json!({"kind":"agent"}),json!({"kind":"agent","cutexSessionId":"cutex.agent","role":"operator"}),json!({"kind":"unknown"})] {
            assert!(serde_json::from_value::<SessionCreatedBy>(bad).is_err());
        }
    }
    #[test]
    fn session_creator_is_immutable_and_ordinary_metadata_never_relabels_it() {
        let mut r=record();let actor=director();
        r.stamp_created_by_at_creation(Some(&actor)).unwrap();r.stamp_created_by_at_creation(Some(&actor)).unwrap();
        r.stamp_created_by_at_creation(None).unwrap();
        assert!(r.stamp_created_by_at_creation(Some(&SessionCreatedBy::Owner)).is_err());
        crate::session::metadata::apply_session_metadata_patch(&mut r,&json!({"note":"updated later","createdBy":{"kind":"owner"}})).unwrap();
        assert_eq!(r.created_by,Some(actor));
    }
    #[test]
    fn session_creator_explicit_path_store_roundtrip_keeps_unknown_and_captured_origins() {
        let root=std::env::temp_dir().join(format!("session-created-by-{}",uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();let path=root.join("cutex-sessions.json");
        let mut store=crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
        let creators=[None,Some(SessionCreatedBy::Owner),Some(SessionCreatedBy::System),Some(director())];
        for (n,creator) in creators.iter().enumerate() {
            let mut r=CutexSessionRecord::new(format!("cutex.creator-{n}"),None,"host".into(),root.to_string_lossy().into(),None).unwrap();
            r.stamp_created_by_at_creation(creator.as_ref()).unwrap();store.sessions.insert(r.cutex_session_id.clone(),r);
        }
        crate::session::store::save_cutex_session_store_to_path(&path,&store).unwrap();
        let restored=crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
        for (n,creator) in creators.iter().enumerate() {assert_eq!(&restored.sessions[&format!("cutex.creator-{n}")].created_by,creator);}
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_creator_restore_roundtrip_preserves_original_role_and_fork_uses_new_actor() {
        let mut source=record();source.stamp_created_by_at_creation(Some(&director())).unwrap();
        let restored:CutexSessionRecord=serde_json::from_value(serde_json::to_value(&source).unwrap()).unwrap();
        assert_eq!(restored.created_by,source.created_by);
        let mut fork=CutexSessionRecord::from_codex_session_id("018a0000-0000-7000-8000-00000000c259").unwrap();fork.stamp_created_by_at_creation(Some(&SessionCreatedBy::Owner)).unwrap();
        crate::session::fork::inherit_fork_avatar(&source,&mut fork);
        assert_eq!(fork.created_by,Some(SessionCreatedBy::Owner),"new fork's creator is the action actor, never its source's creator");
        assert_eq!(source.created_by,Some(director()));
    }
}
