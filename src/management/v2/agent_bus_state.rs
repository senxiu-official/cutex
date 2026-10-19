use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::agent_bus::model::AgentBusMessage;
use crate::app_server::commands::InterAgentContextPersistedReceipt;
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;
use crate::management::v2::model::CutexMessage;
use crate::management::v2::model::EventCorrelation;
use crate::management::v2::model::EventSource;
use crate::management::v2::model::PendingEvent;
use crate::management::v2::repository::management_v2_repository;
use crate::platform::host::current_host_name;

const AGENT_BUS_STATE_FILE: &str = "agent-bus-message-state.json";
const AGENT_BUS_LOCK_FILE: &str = "agent-bus-message-state.lock";

fn is_false(value: &bool) -> bool {
    !*value
}

static AGENT_BUS_MESSAGE_REPOSITORY: super::process_repository::ProcessRepository<
    AgentBusMessageRepository,
> = super::process_repository::ProcessRepository::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentBusMessageSnapshot {
    pub message_id: String,
    pub from_cutex_session_id: Option<String>,
    pub to_cutex_session_id: String,
    pub delivery_mode: String,
    pub content: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale_release_authorized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2_submission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a4_receipt: Option<InterAgentContextPersistedReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_inbox_receipt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_input: Option<crate::app_server::external_input::Envelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_input_receipt: Option<crate::app_server::external_input::Receipt>,
    /// Observation only, not a task result or an automatic retry permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_input_last_observed: Option<crate::app_server::external_input::Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_input_commit_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<crate::app_server::presentation::Obligation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAgentBusMessage {
    owner_cutex_session_id: String,
    from_runtime_agent_id: Option<String>,
    to_runtime_agent_id: Option<String>,
    #[serde(default)]
    canonical_envelope: Option<AgentBusMessage>,
    snapshot: AgentBusMessageSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queued_at: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    admission_started_idle: bool,
    updated_at: String,
    /// Display facts (agent messaging v2). Additive: older binaries ignore
    /// it and drop it on their next rewrite, which only loses display data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<AgentBusMessageMeta>,
}

/// One side of an agent message as clients see it (`AgentParty`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentParty {
    pub id: String,
    pub name: Option<String>,
    /// `codex` or `claude`.
    pub runtime: Option<String>,
}

impl AgentParty {
    /// The party of a durable session from the session store; never
    /// self-reported. Unknown sessions keep only their id.
    pub fn of_session(sessions: Option<&crate::session::model::CutexSessionStore>, id: &str) -> Self {
        let record = sessions.and_then(|store| store.sessions.get(id));
        Self {
            id: id.to_string(),
            name: record
                .and_then(|r| r.formal_agent_name.clone().or_else(|| r.display_name_override.clone()))
                .map(|name| sanitize_party_name(&name))
                .filter(|name| !name.is_empty()),
            runtime: record.map(|r| if r.claude_session.is_some() { "claude" } else { "codex" }.to_string()),
        }
    }
}

/// A display name safe for a one-line model header: no control characters,
/// trimmed, at most 160 characters.
pub fn sanitize_party_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

/// Per-message display facts kept beside the canonical record (agent
/// messaging v2). Not part of the semantic digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBusMessageMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<AgentParty>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<AgentParty>,
    #[serde(default)]
    pub facts: crate::agent_bus::model::AgentSenderFacts,
    /// The recipient turn the message was read into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentBusMessageStore {
    version: u8,
    #[serde(default)]
    external_recovery_actions:
        BTreeMap<String, crate::app_server::external_recovery::RecoveryReceipt>,
    #[serde(default)]
    messages: BTreeMap<String, StoredAgentBusMessage>,
}

pub struct AgentBusMessageRepository {
    root: PathBuf,
    process_lock: Mutex<()>,
    cache: Mutex<Option<CachedStore>>,
    admission_index: Mutex<Option<AdmissionIndex>>,
}

#[derive(Debug, Clone)]
pub struct AgentBusQueuedMessage {
    pub owner_cutex_session_id: String,
    pub message_id: String,
    pub from_cutex_session_id: Option<String>,
    pub to_cutex_session_id: String,
    pub from_runtime_agent_id: Option<String>,
    pub to_runtime_agent_id: Option<String>,
    pub delivery_mode: String,
    pub content: String,
    pub queued_at: DateTime<Utc>,
    pub canonical_envelope: AgentBusMessage,
    pub semantic_sha256: String,
}

/// One Agent Management start message ([`AgentBusMessageRepository::management_start_messages`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ManagementStartMessage {
    pub message_id: String,
    pub external_message_id: String,
    pub to_cutex_session_id: String,
    pub state: String,
    pub error: Option<Value>,
    /// The recipient's runtime may hold it
    /// ([`AgentBusMessageSnapshot::may_have_reached_runtime`]).
    pub native_submission_present: bool,
}

#[derive(Debug, Clone)]
pub struct PendingAgentBusMessage {
    pub owner_cutex_session_id: String,
    pub target_cutex_session_id: String,
    pub canonical_envelope: AgentBusMessage,
    pub semantic_sha256: String,
    pub native_submission_present: bool,
    pub stale_release_authorized: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBusBacklogAnalysis {
    pub message_id: String,
    pub state: String,
    pub semantic_sha256: Option<String>,
    pub created_at_epoch_secs: u64,
    pub age_secs: Option<u64>,
    pub sender_kind: String,
    pub control_type: Option<String>,
    pub delivery_mode: String,
    pub native_submission_present: bool,
    pub protected_control: bool,
    pub classification: String,
    pub reasons: Vec<String>,
    pub content: String,
}

/// Error code of an owner discard (`AgentBusMessageRepository::owner_discard`).
pub const OWNER_DISCARD_CODE: &str = "owner_message_discarded";

/// An undelivered message as the owner's stuck-message list shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StuckAgentBusMessage {
    pub message_id: String,
    pub state: String,
    /// Recipient session (the record owner).
    pub to_cutex_session_id: String,
    pub from_cutex_session_id: Option<String>,
    pub to_runtime_agent_id: Option<String>,
    pub from_runtime_agent_id: Option<String>,
    pub semantic_sha256: Option<String>,
    pub created_at_epoch_secs: Option<u64>,
    pub age_secs: Option<u64>,
    pub updated_at: String,
    pub sender_kind: String,
    pub control_type: Option<String>,
    pub delivery_mode: String,
    pub native_submission_present: bool,
    /// Last observed native ingress status, if any.
    pub native_last_observed: Option<Value>,
    pub protected_control: bool,
    pub error: Option<Value>,
    pub reasons: Vec<String>,
    pub content_preview: String,
}

#[derive(Debug, Clone)]
pub struct OwnerDiscardOutcome {
    /// False when the message was already owner-discarded (idempotent).
    pub changed: bool,
    pub previous_state: String,
    pub owner_cutex_session_id: String,
    pub to_runtime_agent_id: Option<String>,
    pub snapshot: AgentBusMessageSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerDiscardError {
    NotFound,
    InvalidReason,
    DigestMismatch,
    ProtectedControl,
    NotDiscardable { state: String },
}

impl OwnerDiscardError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "message_not_found",
            Self::InvalidReason => "invalid_request",
            Self::DigestMismatch => "message_digest_mismatch",
            Self::ProtectedControl => "message_protected",
            Self::NotDiscardable { .. } => "message_not_discardable",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NotFound => "no durable agent-bus message has this id".into(),
            Self::InvalidReason => "a nonempty reason of at most 1024 bytes is required".into(),
            Self::DigestMismatch => "the message's semantic digest differs from expectedSemanticSha256".into(),
            Self::ProtectedControl => "Task / Job / control messages need allowProtected: check the assignment or job first".into(),
            Self::NotDiscardable { state } => format!("only pending or stale_held messages can be discarded (state {state})"),
        }
    }
}

/// Owner-discarded: this route's code, or a message the r214 owner script
/// (a one-off owner maintenance script, drop-stuck-messages.sh) marked
/// `quarantined` with `{source: human, code: stale_message_discarded}`.
fn is_owner_discarded(snapshot: &AgentBusMessageSnapshot) -> bool {
    let error = snapshot.error.as_ref();
    let field = |name: &str| error.and_then(|e| e.get(name)).and_then(Value::as_str);
    match snapshot.state.as_str() {
        "discarded" => field("code") == Some(OWNER_DISCARD_CODE),
        "quarantined" => {
            field("code") == Some(OWNER_DISCARD_CODE)
                || (field("source") == Some("human")
                    && field("code") == Some("stale_message_discarded"))
        }
        _ => false,
    }
}

impl AgentBusMessageSnapshot {
    /// Whether the recipient's runtime may already hold this message: a
    /// native submission / receipt / frozen ExternalInput, a Claude
    /// local-inbox acceptance, or an owner discard that recorded such
    /// evidence. Not a delivery receipt (only `delivered` is one).
    pub fn may_have_reached_runtime(&self) -> bool {
        native_evidence(self)
            || self.local_inbox_receipt.is_some()
            || self
                .error
                .as_ref()
                .and_then(|error| error.pointer("/details/nativeSubmissionPresent"))
                .and_then(Value::as_bool)
                == Some(true)
    }
}

fn native_evidence(snapshot: &AgentBusMessageSnapshot) -> bool {
    snapshot.a2_submission_id.is_some()
        || snapshot.a4_receipt.is_some()
        || snapshot.external_input.is_some()
        || snapshot.external_input_receipt.is_some()
}

fn stored_protected_control(stored: &StoredAgentBusMessage) -> bool {
    stored.canonical_envelope.as_ref().is_some_and(|message| {
        message.sender_kind.is_task_service_system()
            || message.sender_kind.is_job_service_system()
            || message.control_type.is_some()
    })
}

fn stored_created_epoch_secs(stored: &StoredAgentBusMessage) -> Option<u64> {
    stored
        .canonical_envelope
        .as_ref()
        .map(|message| message.created_at_epoch_secs)
        .filter(|created| *created > 0)
        .or_else(|| {
            DateTime::parse_from_rfc3339(&stored.updated_at)
                .ok()
                .and_then(|at| u64::try_from(at.timestamp()).ok())
        })
}

fn stuck_view(
    stored: &StoredAgentBusMessage,
    created: Option<u64>,
    age: Option<u64>,
) -> StuckAgentBusMessage {
    let snapshot = &stored.snapshot;
    let canonical = stored.canonical_envelope.as_ref();
    let native = native_evidence(snapshot);
    let protected = stored_protected_control(stored);
    let observed = snapshot
        .external_input_last_observed
        .as_ref()
        .and_then(|status| serde_json::to_value(status).ok());
    let mut reasons = Vec::new();
    if snapshot.state == "stale_held" {
        reasons.push("held at startup redrive: older than the stale threshold, never submitted to the native runtime".to_string());
    }
    if snapshot.external_input_receipt.is_some() || snapshot.a4_receipt.is_some() {
        reasons.push("the native runtime accepted it but delivery was never confirmed".to_string());
    } else if native {
        reasons.push(match &observed {
            Some(status) => format!("frozen for the native runtime, not confirmed (last observed: {status})"),
            None => "frozen for the native runtime, not confirmed".to_string(),
        });
    }
    if let Some(code) = snapshot
        .error
        .as_ref()
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
    {
        reasons.push(format!("last error: {code}"));
    }
    if canonical.is_none() {
        reasons.push("no canonical envelope (legacy record): it cannot be redriven".to_string());
    }
    if protected {
        reasons.push("Task / Job / control message: check its assignment or job before discarding".to_string());
    }
    if native {
        reasons.push("discard quarantines it; the native runtime may still hold a copy".to_string());
    }
    StuckAgentBusMessage {
        message_id: snapshot.message_id.clone(),
        state: snapshot.state.clone(),
        to_cutex_session_id: snapshot.to_cutex_session_id.clone(),
        from_cutex_session_id: snapshot.from_cutex_session_id.clone(),
        to_runtime_agent_id: stored.to_runtime_agent_id.clone(),
        from_runtime_agent_id: stored.from_runtime_agent_id.clone(),
        semantic_sha256: snapshot.semantic_sha256.clone(),
        created_at_epoch_secs: created,
        age_secs: age,
        updated_at: stored.updated_at.clone(),
        sender_kind: canonical
            .map(|message| message.sender_kind.sender_label().to_string())
            .unwrap_or_else(|| "unknown".into()),
        control_type: canonical.and_then(|message| message.control_type.clone()),
        delivery_mode: snapshot.delivery_mode.clone(),
        native_submission_present: native,
        native_last_observed: observed,
        protected_control: protected,
        error: snapshot.error.clone(),
        reasons,
        content_preview: crate::agent_bus::audit::content_preview(&snapshot.content, 200),
    }
}

pub fn agent_bus_message_repository() -> anyhow::Result<&'static AgentBusMessageRepository> {
    #[cfg(test)]
    require_private_test_home()?;
    AGENT_BUS_MESSAGE_REPOSITORY.get_or_open(
        || Ok(runtime_dir()?.join("management-v2")),
        AgentBusMessageRepository::open,
    )
}

#[cfg(test)]
fn require_private_test_home() -> anyhow::Result<()> {
    let home = std::env::var("HOME").context("tests require HOME")?;
    let private = std::env::var("CUTEX_TEST_PRIVATE_HOME")
        .context("tests touching Agent Bus message state require CUTEX_TEST_PRIVATE_HOME")?;
    validate_private_test_home(&home, &private)?;
    if !Path::new(&private)
        .join(".cutex-test-private-home")
        .is_file()
    {
        anyhow::bail!(
            "refusing to open Agent Bus message state outside the verified private test HOME"
        );
    }
    Ok(())
}

#[cfg(test)]
fn validate_private_test_home(home: &str, private: &str) -> anyhow::Result<()> {
    if home.is_empty() || private.is_empty() || home != private || private == "/" {
        anyhow::bail!("Agent Bus repository test HOME is not private and exact");
    }
    Ok(())
}

impl AgentBusMessageRepository {
    /// Includes already ACKed input records. The canonical store has no pruning;
    /// do not derive this worklist from the transport's pending-message queue.
    pub(crate) fn due_presentations(&self, owner: &str, now: i64) -> anyhow::Result<Vec<String>> {
        self.read(|s| {
            let mut due: Vec<_> = s
                .messages
                .values()
                .filter(|m| m.snapshot.to_cutex_session_id == owner)
                .filter_map(|m| {
                    m.snapshot
                        .presentation
                        .as_ref()
                        .filter(|p| p.receipt.is_none() && p.next_attempt_at <= now)
                        .map(|p| (p.next_attempt_at, m.snapshot.message_id.clone()))
                })
                .collect();
            due.sort();
            Ok(due.into_iter().take(4).map(|(_, id)| id).collect())
        })
    }
    pub(crate) fn begin_presentation_attempt(
        &self,
        owner: &str,
        id: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        self.mutate_checked(|s| {
            let m = s
                .messages
                .get_mut(id)
                .context("presentation canonical absent")?;
            anyhow::ensure!(
                m.snapshot.to_cutex_session_id == owner,
                "presentation recipient changed"
            );
            let p = m
                .snapshot
                .presentation
                .as_mut()
                .context("presentation not requested")?;
            anyhow::ensure!(
                p.receipt.is_none() && p.next_attempt_at <= now,
                "presentation attempt not due"
            );
            p.next_attempt_at = now.saturating_add(30);
            Ok(())
        })
    }
    pub(crate) fn freeze_presentation(
        &self,
        owner: &str,
        id: &str,
        thread: &str,
    ) -> anyhow::Result<crate::app_server::presentation::Receipt> {
        self.mutate_checked(|s| {
            let m = s
                .messages
                .get_mut(id)
                .context("presentation canonical absent")?;
            anyhow::ensure!(
                m.snapshot.to_cutex_session_id == owner,
                "presentation recipient changed"
            );
            let p = m
                .snapshot
                .presentation
                .as_mut()
                .context("presentation not requested")?;
            anyhow::ensure!(
                matches!(p.version, 1 | 2)
                    && m.snapshot.semantic_sha256.as_deref() == Some(&p.canonical_sha256),
                "presentation canonical conflict"
            );
            let input = m
                .snapshot
                .external_input
                .as_ref()
                .context("presentation waiting for input binding")?;
            let a4 = m
                .snapshot
                .external_input_receipt
                .as_ref()
                .context("presentation waiting for input Commit")?;
            input.validate()?;
            a4.validate(
                &crate::launch::stock::ExternalInputBinding {
                    version: 1,
                    owner_id: input.owner_id.clone(),
                    thread_id: input.thread_id.clone(),
                    runtime_generation: input.runtime_generation,
                    canonical_byte_limit: Default::default(),
                },
                &input.key(),
            )?;
            anyhow::ensure!(
                input.owner_id == owner
                    && input.thread_id == thread
                    && a4.owner_id == owner
                    && a4.thread_id == thread
                    && a4.message_id == id,
                "presentation input reference conflict"
            );
            let expected = crate::app_server::presentation::Receipt::prepare(
                owner.into(),
                thread.into(),
                p.presentation.clone(),
            )?;
            if let Some(old) = &p.frozen {
                anyhow::ensure!(
                    old == &expected,
                    "presentation frozen target/payload conflict"
                );
            } else {
                p.frozen = Some(expected.clone());
            }
            Ok(expected)
        })
    }
    pub(crate) fn commit_presentation(
        &self,
        owner: &str,
        id: &str,
        expected: &crate::app_server::presentation::Receipt,
        receipt: &crate::app_server::presentation::Receipt,
        generation: u64,
    ) -> anyhow::Result<()> {
        receipt.validate()?;
        anyhow::ensure!(
            expected == receipt && receipt.owner_id == owner,
            "presentation receipt mismatch"
        );
        self.mutate_checked(|s| {
            let m = s
                .messages
                .get_mut(id)
                .context("presentation canonical absent")?;
            anyhow::ensure!(
                m.snapshot.to_cutex_session_id == owner,
                "presentation recipient changed"
            );
            let p = m
                .snapshot
                .presentation
                .as_mut()
                .context("presentation not requested")?;
            anyhow::ensure!(
                p.frozen.as_ref() == Some(expected)
                    && m.snapshot.semantic_sha256.as_deref() == Some(&p.canonical_sha256)
                    && p.receipt.as_ref().is_none_or(|r| r == receipt),
                "presentation commit CAS conflict"
            );
            p.receipt = Some(receipt.clone());
            p.commit_generation.get_or_insert(generation);
            p.last_error = None;
            // Deliberately leave input state/A4/Job ACK unchanged.
            Ok(())
        })
    }
    pub(crate) fn presentation_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        self.mutate_checked(|s| {
            let p = s
                .messages
                .get_mut(id)
                .and_then(|m| m.snapshot.presentation.as_mut())
                .context("presentation absent")?;
            if p.receipt.is_none() {
                p.last_error = Some(error.chars().take(512).collect());
            }
            Ok(())
        })
    }
    pub(crate) fn recovery_action(
        &self,
        action: &str,
    ) -> anyhow::Result<Option<crate::app_server::external_recovery::RecoveryReceipt>> {
        self.read(|s| Ok(s.external_recovery_actions.get(action).cloned()))
    }

    pub(crate) fn save_recovery_action(
        &self,
        receipt: &crate::app_server::external_recovery::RecoveryReceipt,
    ) -> anyhow::Result<()> {
        self.mutate_checked(|s| {
            if let Some(old) = s.external_recovery_actions.get(&receipt.action_id) {
                anyhow::ensure!(
                    old.review == receipt.review && old.retry_id == receipt.retry_id,
                    "recovery action semantic conflict"
                );
                anyhow::ensure!(
                    old.result.is_none() || old.result == receipt.result,
                    "recovery receipt conflict"
                );
            }
            s.external_recovery_actions
                .insert(receipt.action_id.clone(), receipt.clone());
            s.version = s.version.max(4);
            Ok(())
        })
    }
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        secure_directory(&root)?;
        Ok(Self {
            root,
            process_lock: Mutex::new(()),
            cache: Mutex::new(None),
            admission_index: Mutex::new(None),
        })
    }

    /// Returns `true` only for the first durable commit and `false` for an
    /// exact replay of the same message identity and semantic content.
    pub fn record_queued(&self, message: AgentBusQueuedMessage) -> anyhow::Result<bool> {
        self.record_queued_internal(message, true, None, None)
    }

    /// Like [`Self::record_queued`], with the sender's display facts.
    pub fn record_queued_with_meta(
        &self,
        message: AgentBusQueuedMessage,
        meta: AgentBusMessageMeta,
    ) -> anyhow::Result<bool> {
        self.record_queued_internal(message, true, None, Some(meta))
    }

    /// Only the authenticated Job handler calls this after its source checks.
    /// Existing records never gain an obligation on replay/config change.
    pub fn record_queued_private_job(
        &self,
        message: AgentBusQueuedMessage,
        policy: Option<&crate::app_server::presentation::PrivateJobPolicy>,
    ) -> anyhow::Result<bool> {
        let display = match policy {
            Some(p) => p.obligation(&message.canonical_envelope, &message.semantic_sha256)?,
            _ => None,
        };
        self.record_queued_internal(message, true, display, None)
    }

    #[cfg(test)]
    pub(crate) fn record_queued_isolated(&self, message: AgentBusQueuedMessage) -> anyhow::Result<bool> {
        self.record_queued_internal(message, false, None, None)
    }

    fn record_queued_internal(
        &self,
        message: AgentBusQueuedMessage,
        record_management_event: bool,
        presentation: Option<crate::app_server::presentation::Obligation>,
        meta: Option<AgentBusMessageMeta>,
    ) -> anyhow::Result<bool> {
        validate_session_identity(&message.owner_cutex_session_id)?;
        if let Some(from) = message.from_cutex_session_id.as_deref() {
            validate_session_identity(from)?;
        }
        validate_session_identity(&message.to_cutex_session_id)?;
        if message.message_id.is_empty() {
            anyhow::bail!("agent-bus messageId must not be empty");
        }
        validate_semantic_sha256(&message.semantic_sha256)?;
        if message.canonical_envelope.id != message.message_id
            || message.canonical_envelope.to_cutex_session_id.as_deref()
                != Some(message.to_cutex_session_id.as_str())
        {
            anyhow::bail!("agent-bus canonical envelope identity is inconsistent");
        }
        if !matches!(
            message.delivery_mode.as_str(),
            "after_turn" | "soon" | "passive"
        ) {
            anyhow::bail!("agent-bus delivery mode is outside the v2 contract");
        }
        self.update(|store| {
            if let Some(existing) = store.messages.get(&message.message_id) {
                if existing.owner_cutex_session_id == message.owner_cutex_session_id
                    && existing.snapshot.from_cutex_session_id == message.from_cutex_session_id
                    && existing.snapshot.to_cutex_session_id == message.to_cutex_session_id
                    && existing.snapshot.delivery_mode == message.delivery_mode
                    && existing.snapshot.content == message.content
                    && existing.snapshot.semantic_sha256.as_deref()
                        == Some(message.semantic_sha256.as_str())
                {
                    return Ok((false, false));
                }
                anyhow::bail!("agent-bus messageId was reused with different canonical content");
            }
            // Agent messages, and cross-host tells (no local sender session;
            // `remote` names where they came from).
            let peer = peer_tell_fields(message.from_cutex_session_id.as_deref(), &message.canonical_envelope);
            if record_management_event && (message.from_cutex_session_id.is_some() || peer.is_some()) {
                let mut params = with_agent_fields(
                    json!({
                        "messageId": message.message_id,
                        "fromCutexSessionId": message.from_cutex_session_id,
                        "toCutexSessionId": message.to_cutex_session_id,
                        "fromRuntimeAgentId": message.from_runtime_agent_id,
                        "toRuntimeAgentId": message.to_runtime_agent_id,
                        "deliveryMode": message.delivery_mode,
                        "content": message.content,
                        "queuedAt": message.queued_at.to_rfc3339(),
                    }),
                    &message.delivery_mode,
                    &message.content,
                    message.from_cutex_session_id.as_deref(),
                    &message.to_cutex_session_id,
                    meta.as_ref(),
                );
                if let (Some(object), Some(peer)) = (params.as_object_mut(), peer) {
                    object.extend(peer);
                }
                append_event(
                    &message.owner_cutex_session_id,
                    &message.message_id,
                    "cutex/agentBus/messageQueued",
                    params,
                )?;
            }
            if message.canonical_envelope.control_type.as_deref()
                == Some(crate::agent_bus::job_completion::SCHEMA_V2)
            {
                let frozen = crate::agent_bus::job_completion::FrozenProjection::from_message(
                    &message.canonical_envelope,
                )?;
                if record_management_event {
                    super::integration_events::ensure_job_terminal_observed(&frozen.request)?;
                }
                store.version = store.version.max(6);
            } else if presentation.is_some() {
                store.version = store.version.max(5);
            }
            store.messages.insert(
                message.message_id.clone(),
                StoredAgentBusMessage {
                    owner_cutex_session_id: message.owner_cutex_session_id,
                    from_runtime_agent_id: message.from_runtime_agent_id,
                    to_runtime_agent_id: message.to_runtime_agent_id,
                    canonical_envelope: Some(message.canonical_envelope),
                    snapshot: AgentBusMessageSnapshot {
                        message_id: message.message_id,
                        from_cutex_session_id: message.from_cutex_session_id,
                        to_cutex_session_id: message.to_cutex_session_id,
                        delivery_mode: message.delivery_mode,
                        content: message.content,
                        state: "pending".to_string(),
                        stale_release_authorized: false,
                        semantic_sha256: Some(message.semantic_sha256),
                        a2_submission_id: None,
                        a4_receipt: None,
                        local_inbox_receipt: None,
                        external_input: None,
                        external_input_receipt: None,
                        external_input_last_observed: None,
                        external_input_commit_generation: None,
                        presentation,
                        error: None,
                    },
                    admission_started_idle: false,
                    queued_at: Some(message.queued_at.to_rfc3339()),
                    updated_at: message.queued_at.to_rfc3339(),
                    meta,
                },
            );
            Ok((true, true))
        })
    }

    /// Freeze from the authenticated, persisted canonical record, never a poll
    /// payload. V3 prevents older writers from dropping native receipt fields.
    pub fn freeze_external_input(
        &self,
        owner: &str,
        message_id: &str,
        make: impl FnOnce(
            &AgentBusMessage,
        ) -> anyhow::Result<crate::app_server::external_input::Envelope>,
    ) -> anyhow::Result<crate::app_server::external_input::Envelope> {
        // Every delivery sweep of a retained-pending message (a passive one
        // waits natively for the recipient's next turn) calls this again: an
        // already frozen envelope changes nothing, so the whole store is not
        // serialized just to learn that (47 MB per call on the Linux host,
        // r240 idle CPU, 2026-10-15).
        self.update_inner(true, |store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("canonical message absent; explicit review required")?;
            anyhow::ensure!(
                stored.snapshot.to_cutex_session_id == owner,
                "canonical recipient changed"
            );
            if let Some(envelope) = &stored.snapshot.external_input {
                envelope.validate()?;
                return Ok((envelope.clone(), false));
            }
            anyhow::ensure!(
                stored.snapshot.state == "pending"
                    && stored.snapshot.a2_submission_id.is_none()
                    && stored.snapshot.a4_receipt.is_none(),
                "legacy native submission ambiguous; explicit review required"
            );
            let canonical = stored
                .canonical_envelope
                .as_ref()
                .context("legacy canonical envelope absent; explicit review required")?;
            let envelope = make(canonical)?;
            envelope.validate()?;
            anyhow::ensure!(
                envelope.owner_id == owner
                    && envelope.message.id == canonical.id
                    && canonical.id == message_id,
                "native envelope recipient/message mismatch"
            );
            stored.snapshot.external_input = Some(envelope.clone());
            store.version = store.version.max(if envelope.version == 2 {
                6
            } else if envelope.message.delivery == crate::app_server::external_input::Delivery::Soon
            {
                4
            } else {
                3
            });
            Ok((envelope, true))
        })
    }

    /// Input overflow, spill at delivery: replace a frozen envelope the
    /// native never took (the caller saw its status unknown, or a size
    /// refusal) with its overlength form. Compare-and-swap on the frozen
    /// envelope; only the text (and so the digest) may change.
    pub(crate) fn replace_frozen_external_input(
        &self,
        owner: &str,
        message_id: &str,
        frozen: &crate::app_server::external_input::Envelope,
        spilled: &crate::app_server::external_input::Envelope,
    ) -> anyhow::Result<()> {
        spilled.validate()?;
        anyhow::ensure!(
            spilled.owner_id == frozen.owner_id
                && spilled.thread_id == frozen.thread_id
                && spilled.version == frozen.version
                && spilled.view == frozen.view
                && spilled.message.id == frozen.message.id
                && spilled.message.source == frozen.message.source
                && spilled.message.event_type == frozen.message.event_type
                && spilled.message.delivery == frozen.message.delivery,
            "spilled native envelope differs beyond its text"
        );
        self.mutate(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("canonical message absent; explicit review required")?;
            anyhow::ensure!(
                stored.snapshot.to_cutex_session_id == owner
                    && stored.snapshot.state == "pending"
                    && stored.snapshot.a2_submission_id.is_none()
                    && stored.snapshot.a4_receipt.is_none()
                    && stored.snapshot.external_input.as_ref() == Some(frozen),
                "frozen native envelope changed; not replaced"
            );
            stored.snapshot.external_input = Some(spilled.clone());
            stored.snapshot.external_input_last_observed = None;
            Ok(())
        })
    }

    /// Caller holds current lifecycle/occurrence fences. The frozen envelope
    /// and original receipt are compared again under this repository's lock.
    pub(crate) fn record_external_input_delivered(
        &self,
        owner: &str,
        message_id: &str,
        envelope: &crate::app_server::external_input::Envelope,
        receipt: &crate::app_server::external_input::Receipt,
        generation: u64,
    ) -> anyhow::Result<()> {
        self.mutate_checked(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("canonical message disappeared")?;
            anyhow::ensure!(
                stored.snapshot.to_cutex_session_id == owner
                    && stored.snapshot.external_input.as_ref() == Some(envelope),
                "native business commit CAS conflict"
            );
            anyhow::ensure!(
                stored.snapshot.state == "pending" || stored.snapshot.state == "delivered",
                "native business state conflict"
            );
            anyhow::ensure!(
                stored
                    .snapshot
                    .external_input_receipt
                    .as_ref()
                    .is_none_or(|r| r == receipt),
                "native receipt replay conflict"
            );
            receipt.validate(
                &crate::launch::stock::ExternalInputBinding {
                    version: 1,
                    owner_id: envelope.owner_id.clone(),
                    thread_id: envelope.thread_id.clone(),
                    runtime_generation: envelope.runtime_generation,
                    canonical_byte_limit: Default::default(),
                },
                &envelope.key(),
            )?;
            let first_delivery = stored.snapshot.state == "pending";
            let now = Utc::now().to_rfc3339();
            stored.snapshot.external_input_receipt = Some(receipt.clone());
            stored
                .snapshot
                .external_input_commit_generation
                .get_or_insert(generation);
            stored.snapshot.state = "delivered".into();
            stored.snapshot.error = None;
            stored.updated_at = now.clone();
            if !first_delivery {
                return Ok(None);
            }
            let meta = stored.meta.get_or_insert_with(Default::default);
            meta.context_turn_id = Some(receipt.turn_id.clone());
            meta.delivered_at = Some(now.clone());
            Ok(delivered_event(stored, Some(receipt.receipt_id.clone()), &receipt.turn_id, "codex", &now))
        })
        .and_then(|event| emit_both(event))
    }

    /// Claude: the adapter emitted the message's timeline item in `turn_id`
    /// (read into context). Idempotent; emits `messageDelivered` once.
    pub fn record_context_injected(
        &self,
        owner: &str,
        message_id: &str,
        turn_id: &str,
    ) -> anyhow::Result<bool> {
        let event = self.mutate_checked(|store| {
            let Some(stored) = store.messages.get_mut(message_id) else {
                return Ok(None);
            };
            anyhow::ensure!(stored.owner_cutex_session_id == owner, "agent-bus message owner mismatch");
            if !matches!(stored.snapshot.state.as_str(), "pending" | "inbox_accepted") {
                return Ok(None);
            }
            let now = Utc::now().to_rfc3339();
            stored.snapshot.state = "delivered".into();
            stored.snapshot.error = None;
            stored.updated_at = now.clone();
            let meta = stored.meta.get_or_insert_with(Default::default);
            meta.context_turn_id = Some(turn_id.to_string());
            meta.delivered_at = Some(now.clone());
            Ok(delivered_event(stored, None, turn_id, "claude", &now))
        })?;
        let delivered = event.is_some();
        emit_both(event)?;
        Ok(delivered)
    }

    /// Agent notes to `cutex_session_id` not yet read into its context.
    pub fn pending_note_count(&self, cutex_session_id: &str) -> anyhow::Result<usize> {
        self.read(|store| {
            Ok(store
                .messages
                .values()
                .filter(|m| {
                    m.snapshot.to_cutex_session_id == cutex_session_id
                        && (m.snapshot.from_cutex_session_id.is_some()
                            || m.canonical_envelope.as_ref().is_some_and(crate::agent_bus::peer_tell::is_peer_tell))
                        && m.snapshot.delivery_mode == "passive"
                        && matches!(m.snapshot.state.as_str(), "pending" | "inbox_accepted" | "stale_held")
                })
                .count())
        })
    }

    /// The message's state and whether a local inbox receipt is recorded.
    pub fn message_state(&self, message_id: &str) -> anyhow::Result<Option<(String, bool)>> {
        Ok(self.get(message_id)?.map(|stored| {
            (stored.snapshot.state.clone(), stored.snapshot.local_inbox_receipt.is_some())
        }))
    }

    /// Display facts of a message (agent messaging v2), if recorded.
    pub fn message_meta(&self, message_id: &str) -> anyhow::Result<Option<AgentBusMessageMeta>> {
        Ok(self.get(message_id)?.and_then(|stored| stored.meta))
    }

    /// Messages `cutex_session_id` sent (latest `limit`, oldest first).
    pub fn sent_snapshot(&self, cutex_session_id: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        self.read(|store| {
            let mut sent: Vec<&StoredAgentBusMessage> = store
                .messages
                .values()
                .filter(|m| m.snapshot.from_cutex_session_id.as_deref() == Some(cutex_session_id))
                .collect();
            sent.sort_by_key(|m| {
                (
                    m.canonical_envelope.as_ref().map_or(0, |c| c.created_at_epoch_secs),
                    m.snapshot.message_id.clone(),
                )
            });
            let skip = sent.len().saturating_sub(limit);
            Ok(sent
                .into_iter()
                .skip(skip)
                .map(|m| {
                    let fields = stored_agent_fields(m);
                    let mut entry = json!({
                        "messageId": m.snapshot.message_id,
                        "toCutexSessionId": m.snapshot.to_cutex_session_id,
                        "recipient": fields.get("recipient"),
                        "deliveryMode": m.snapshot.delivery_mode,
                        "deliveryKind": fields.get("deliveryKind"),
                        "body": m.snapshot.content,
                        "state": m.snapshot.state,
                        "sentAt": sent_at(m),
                    });
                    if let Some(meta) = &m.meta {
                        if let Some(turn) = &meta.context_turn_id {
                            entry["contextTurnId"] = json!(turn);
                        }
                        if let Some(at) = &meta.delivered_at {
                            entry["deliveredAt"] = json!(at);
                        }
                        if meta.facts.wake_downgraded {
                            entry["wakeDowngraded"] = json!(true);
                        }
                        if let Some(turn) = &meta.facts.sender_turn_id {
                            entry["senderTurnId"] = json!(turn);
                        }
                    }
                    if let Some(error) = &m.snapshot.error {
                        entry["error"] = error.clone();
                    }
                    entry
                })
                .collect())
        })
    }

    /// The newest wake `from → to` that was read into `to`'s turn
    /// `turn_id` (wake loop guard), with its chain count.
    pub fn wake_into_turn(&self, from: &str, to: &str, turn_id: &str) -> anyhow::Result<Option<u32>> {
        self.read(|store| {
            Ok(store
                .messages
                .values()
                .filter(|m| {
                    m.snapshot.from_cutex_session_id.as_deref() == Some(from)
                        && m.snapshot.to_cutex_session_id == to
                        && m.snapshot.delivery_mode != "passive"
                        && m.meta.as_ref().and_then(|meta| meta.context_turn_id.as_deref()) == Some(turn_id)
                })
                .map(|m| m.meta.as_ref().and_then(|meta| meta.facts.wake_chain).unwrap_or(1))
                .max())
        })
    }

    pub(crate) fn record_external_input_error(
        &self,
        message_id: &str,
        error: &str,
    ) -> anyhow::Result<()> {
        let desired = json!({"code":"external_input_pending", "message":error});
        // Called after every retained-pending delivery attempt: the usual
        // same-error case reads the cached state and writes nothing.
        self.update_inner(true, |store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("canonical message absent")?;
            if stored.snapshot.state == "pending" {
                if stored.snapshot.error.as_ref() == Some(&desired) {
                    return Ok(((), false));
                }
                stored.snapshot.error = Some(desired);
                stored.updated_at = Utc::now().to_rfc3339();
                return Ok(((), true));
            }
            Ok(((), false))
        })
    }

    pub(crate) fn observe_external_input(
        &self,
        message_id: &str,
        frozen: &crate::app_server::external_input::Envelope,
        observed: &crate::app_server::external_input::Status,
    ) -> anyhow::Result<()> {
        // Repeated sweeps usually observe the same native status: no change,
        // no whole-store serialization.
        self.update_inner(true, |store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("native business record absent")?;
            anyhow::ensure!(
                stored.snapshot.external_input.as_ref() == Some(frozen)
                    && observed.message_id == frozen.message.id
                    && observed.semantic_sha256 == frozen.semantic_sha256,
                "native observation CAS conflict"
            );
            if stored.snapshot.external_input_last_observed.as_ref() == Some(observed) {
                return Ok(((), false));
            }
            stored.snapshot.external_input_last_observed = Some(observed.clone());
            Ok(((), true))
        })
    }

    pub fn record_delivered(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
        receipt: &InterAgentContextPersistedReceipt,
        delivered_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let Some(stored) = self.get(message_id)? else {
            return Ok(false);
        };
        if stored.owner_cutex_session_id != owner_cutex_session_id {
            anyhow::bail!("agent-bus message owner session changed");
        }
        if stored.snapshot.state == "delivered" {
            if stored.snapshot.a4_receipt.as_ref() == Some(receipt) {
                return Ok(true);
            }
            anyhow::bail!("agent-bus message A4 receipt changed after delivery");
        }
        // The legacy projection keeps the A4 receipt id as nativeSubmissionId;
        // the v2 ledger holds the complete receipt.
        let event = self.mutate(|store| {
            let stored = store.messages.get_mut(message_id).with_context(|| {
                format!("agent-bus v2 message state disappeared for {message_id}")
            })?;
            stored.snapshot.state = "delivered".to_string();
            stored.snapshot.a4_receipt = Some(receipt.clone());
            stored.snapshot.error = None;
            stored.updated_at = delivered_at.to_rfc3339();
            let meta = stored.meta.get_or_insert_with(Default::default);
            meta.context_turn_id = Some(receipt.turn_id.clone());
            meta.delivered_at = Some(delivered_at.to_rfc3339());
            Ok(delivered_event(stored, Some(receipt.receipt_id.clone()), &receipt.turn_id, "codex", &delivered_at.to_rfc3339()))
        })?;
        emit_both(event)?;
        Ok(true)
    }

    /// Claude's durable inbox acceptance is not native context insertion/A4.
    pub fn record_local_inbox(&self, owner: &str, message: &str, receipt: &Value) -> anyhow::Result<()> {
        anyhow::ensure!(receipt["messageId"] == message && receipt["durable"] == true
            && receipt["receiptScope"] == "local-inbox", "invalid local inbox receipt");
        let mut stable = receipt.clone();
        stable.as_object_mut().context("receipt must be an object")?.remove("duplicate");
        // Processing state may progress; immutable identity is the idempotent acceptance proof.
        stable.as_object_mut().unwrap().remove("state");
        self.mutate(|store| {
            let stored = store.messages.get_mut(message).context("message not found")?;
            anyhow::ensure!(stored.owner_cutex_session_id == owner, "message owner mismatch");
            if let Some(previous) = &stored.snapshot.local_inbox_receipt {
                anyhow::ensure!(previous == &stable, "local inbox receipt changed");
            } else {
                // A Claude note can be read into context (`delivered`) before the
                // submit response is recorded.
                anyhow::ensure!(matches!(stored.snapshot.state.as_str(), "pending" | "delivered"), "message is no longer pending");
                stored.snapshot.local_inbox_receipt = Some(stable);
                if stored.snapshot.state == "pending" {
                    stored.snapshot.state = "inbox_accepted".into();
                }
                stored.updated_at = Utc::now().to_rfc3339();
            }
            Ok(())
        })
    }

    pub fn record_a2_submission(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
        native_submission_id: &str,
        submitted_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let Some(stored) = self.get(message_id)? else {
            return Ok(false);
        };
        if stored.owner_cutex_session_id != owner_cutex_session_id {
            anyhow::bail!("agent-bus message owner session changed");
        }
        self.mutate(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("agent-bus message disappeared")?;
            stored.snapshot.a2_submission_id = Some(native_submission_id.to_string());
            stored.updated_at = submitted_at.to_rfc3339();
            Ok(())
        })?;
        Ok(true)
    }

    pub fn record_quarantined(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
        error: Value,
        at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let Some(stored) = self.get(message_id)? else {
            return Ok(false);
        };
        if stored.owner_cutex_session_id != owner_cutex_session_id {
            anyhow::bail!("agent-bus message owner session changed");
        }
        self.mutate(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("agent-bus message disappeared")?;
            let first = stored.snapshot.state != "quarantined";
            stored.snapshot.state = "quarantined".to_string();
            stored.snapshot.error = Some(error.clone());
            stored.updated_at = at.to_rfc3339();
            Ok(if first { failed_event(stored, &error, &at.to_rfc3339()) } else { None })
        })
        .and_then(emit_both)?;
        Ok(true)
    }

    /// Turn-triggering messages still waiting in cutex or the native inbox.
    /// The cached index visits only this recipient; held input stays visible but
    /// cannot prevent another ridealong from starting.
    pub fn pending_admission_messages(
        &self,
        session: &str,
        asks: &crate::asks::store::AskState,
    ) -> anyhow::Result<Vec<super::admission::AdmissionNextStep>> {
        self.read_admission(session, |messages| {
            let now = Utc::now();
            let wait = crate::agent_bus::delivery::wake_busy_wait();
            let held = super::admission::ridealong_held(asks, session, false);
            Ok(messages.filter_map(|stored| {
                let snapshot = &stored.snapshot;
                let message = stored.canonical_envelope.as_ref()?;
                if snapshot.to_cutex_session_id != session
                    || !matches!(snapshot.state.as_str(), "pending" | "inbox_accepted" | "stale_held")
                    || message.kind != crate::agent_bus::model::AgentBusEnvelopeKind::Message
                    || !message.delivery_mode.trigger_turn()
                    || message.is_removed_task_service_message()
                    || snapshot.external_input.as_ref().is_some_and(|envelope|
                        envelope.message.delivery == crate::app_server::external_input::Delivery::Passive)
                {
                    return None;
                }
                if snapshot.external_input.is_none() && message.sender_kind.is_job_service_system()
                    && message.control_type.as_deref() == Some(crate::agent_bus::job_completion::SCHEMA_V2)
                    && crate::agent_bus::job_completion::FrozenProjection::from_message(message).ok().is_some_and(|projection| {
                        let job_id = &projection.request.job_id;
                        asks.asks.values().any(|ask| ask.remote.is_none() && ask.target_cutex_session_id == session
                            && ask.job(job_id).is_some_and(|job| job.cancel.is_some()
                                || (ask.is_open() && ask.awaiting.as_ref().is_some_and(|wait| wait.jobs.contains(job_id)))))
                    })
                {
                    return None;
                }
                let accepted = snapshot.a2_submission_id.is_some() || snapshot.local_inbox_receipt.is_some()
                    || snapshot.external_input_last_observed.as_ref().is_some_and(|status|
                        status.delivery_state != crate::app_server::external_input::DeliveryState::Unknown);
                let detail = ridealong_detail(stored, message, asks);
                let (timing, next_step_at) = bus_admission_timing(message, snapshot.external_input.as_ref(), now, wait);
                Some(super::admission::AdmissionNextStep {
                    id: message.id.clone(),
                    enqueued_at: stored.queued_at.clone().unwrap_or_else(|| DateTime::<Utc>::from_timestamp(
                        message.created_at_epoch_secs.min(i64::MAX as u64) as i64, 0,
                    ).unwrap_or(DateTime::<Utc>::UNIX_EPOCH).to_rfc3339()),
                    eligible: !accepted && snapshot.state != "stale_held" && held.is_none(),
                    timing, next_step_at, held: held.clone(), ask_id: detail.ask_id,
                    delivery_kind: detail.kind.into(), label: Some(detail.label),
                    from: detail.from, from_host: detail.from_host, preview: detail.preview,
                })
            }).collect())
        })
    }

    /// The admission worker only observes external replacements here. An unchanged
    /// file needs neither the cross-process store lock nor an index traversal.
    pub fn refresh_admission_if_changed(&self) -> anyhow::Result<bool> {
        let path = self.root.join(AGENT_BUS_STATE_FILE);
        let stamp = crate::platform::file_stamp::FileStamp::of(&path);
        {
            let index = self.admission_index.lock().map_err(|_| anyhow::anyhow!("agent-bus admission index lock poisoned"))?;
            if index.as_ref().map(|index| &index.stamp) == Some(&stamp)
                || (stamp.is_none() && index.is_none()) {
                return Ok(false);
            }
        }
        self.with_lock(|path| {
            let stamp = crate::platform::file_stamp::FileStamp::of(path);
            let cache = self.cache.lock().map_err(|_| anyhow::anyhow!("agent-bus cache lock poisoned"))?;
            let mut index = self.admission_index.lock().map_err(|_| anyhow::anyhow!("agent-bus admission index lock poisoned"))?;
            if index.as_ref().is_some_and(|index| index.stamp == stamp) { return Ok(false); }
            let mut refreshed = if let Some(cache) = cache.as_ref().filter(|cache| Some(&cache.stamp) == stamp.as_ref()) {
                AdmissionIndex::new(&cache.store, Some(cache.digest))
            } else if stamp.is_some() {
                // Errors and retry timestamps change frequently. Read only admission
                // facts here; hydrate and validate canonical bodies on actual use.
                let bytes = fs::read(path)?;
                let projection: AdmissionStoreProjection = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(matches!(projection.version, 2..=7), "unsupported management v2 agent-bus state version");
                AdmissionIndex::from_projection(projection.messages, Some(store_digest(&bytes)))
            } else {
                AdmissionIndex::from_projection(BTreeMap::new(), None)
            };
            if crate::platform::file_stamp::FileStamp::of(path) == stamp { refreshed.stamp = stamp; }
            refreshed.notify_changes(index.as_ref());
            *index = Some(refreshed);
            Ok(true)
        })
    }

    /// Passive agent notes already accepted by the native inbox are drained
    /// together with an exclusive ask. Unsubmitted bus messages are not yet
    /// part of that turn, even when their delivery mode is passive.
    pub fn pending_admission_notes(&self, session: &str) -> anyhow::Result<Vec<super::admission::AdmissionNote>> {
        self.read_admission(session, |messages| {
            Ok(messages.filter_map(|stored| {
                let snapshot = &stored.snapshot;
                let message = stored.canonical_envelope.as_ref()?;
                if snapshot.state == "stale_held" || !message.sender_kind.is_agent()
                    || message.control_type.is_some()
                    || message.kind != crate::agent_bus::model::AgentBusEnvelopeKind::Message
                {
                    return None;
                }
                let passive = snapshot.external_input.as_ref().map(|envelope|
                    envelope.message.delivery == crate::app_server::external_input::Delivery::Passive)
                    .unwrap_or(message.delivery_mode == crate::agent_bus::delivery::AgentDeliveryMode::Passive);
                let accepted = snapshot.a2_submission_id.is_some() || snapshot.local_inbox_receipt.is_some()
                    || snapshot.external_input_last_observed.as_ref().is_some_and(|status|
                        status.delivery_state == crate::app_server::external_input::DeliveryState::Pending);
                (passive && accepted).then(|| super::admission::AdmissionNote { label: "Agent note".into() })
            }).collect())
        })
    }

    /// Written only by the bridge that held the session input lock, observed
    /// idle, and got native acceptance. A later context receipt binds the turn.
    pub fn record_admission_started_idle(&self, session: &str, id: &str) -> anyhow::Result<()> {
        self.update(|store| {
            let Some(stored) = store.messages.get_mut(id) else { return Ok(((), false)); };
            anyhow::ensure!(stored.snapshot.to_cutex_session_id == session, "admission recipient changed");
            let changed = !stored.admission_started_idle;
            stored.admission_started_idle = true;
            Ok(((), changed))
        })
    }

    /// Restart classification requires both our idle-submit marker and the
    /// matching native context receipt. Old records cannot prove turn origin.
    pub fn admission_started_turn(&self, session: &str, turn: &str) -> anyhow::Result<bool> {
        self.with_admission_index(|_, index| Ok(index.started_turns.get(session).is_some_and(|turns| turns.contains(turn))))
    }

    fn read_admission<T>(
        &self,
        session: &str,
        action: impl FnOnce(&mut dyn Iterator<Item = &StoredAgentBusMessage>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.with_admission_index(|store, index| {
            let ids = index.sessions.get(session);
            action(&mut ids.into_iter().flatten().filter_map(|id| store.messages.get(id)))
        })
    }

    fn with_admission_index<T>(&self, action: impl FnOnce(&AgentBusMessageStore, &AdmissionIndex) -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.with_lock(|path| {
            let mut cache = self.cache.lock().map_err(|_| anyhow::anyhow!("agent-bus cache lock poisoned"))?;
            let before = crate::platform::file_stamp::FileStamp::of(path);
            let (store, digest) = fresh_store(&mut cache, path)?;
            let after = crate::platform::file_stamp::FileStamp::of(path);
            let mut index = self.admission_index.lock().map_err(|_| anyhow::anyhow!("agent-bus admission index lock poisoned"))?;
            if index.as_ref().is_none_or(|index| index.digest != digest) {
                let mut refreshed = AdmissionIndex::new(&store, digest);
                if before == after { refreshed.stamp = before; }
                refreshed.notify_changes(index.as_ref());
                *index = Some(refreshed);
            }
            action(&store, index.as_ref().unwrap())
        })
    }

    /// Rollback step (services stopped; `src/upgrade/older_stores.rs`): every
    /// cross-host wake (`cutex.peer.tell.v2`, `after_turn`) that is still
    /// pending and was never handed to a runtime becomes the cross-host note
    /// (`cutex.peer.tell.v1`, passive) a release without cross-host wakes
    /// delivers. Such a release skips or keeps retrying an unknown control
    /// type and lists the wake as an eligible next step forever, which holds
    /// owner input ~30 s per message and new delegations to that agent
    /// indefinitely. The digest is recomputed for the note's text; the
    /// message id, sender and facts stay. A wake already frozen for a runtime
    /// (or with any native receipt) is left as it is. Returns the canonical
    /// records as they were before (to keep aside).
    pub fn downgrade_pending_peer_wakes(&self) -> anyhow::Result<Vec<Value>> {
        use crate::agent_bus::delivery::AgentDeliveryMode;
        use crate::agent_bus::peer_tell::{CONTROL_TYPE, CONTROL_TYPE_V2};
        self.update(|store| {
            let mut before = Vec::new();
            for (id, stored) in &mut store.messages {
                let snapshot = &stored.snapshot;
                let Some(canonical) = stored.canonical_envelope.as_ref() else {
                    continue;
                };
                if canonical.control_type.as_deref() != Some(CONTROL_TYPE_V2)
                    || snapshot.state != "pending"
                    || snapshot.external_input.is_some()
                    || snapshot.external_input_receipt.is_some()
                    || snapshot.a2_submission_id.is_some()
                    || snapshot.a4_receipt.is_some()
                    || snapshot.local_inbox_receipt.is_some()
                {
                    continue;
                }
                let mut note = canonical.clone();
                note.control_type = Some(CONTROL_TYPE.into());
                note.delivery_mode = AgentDeliveryMode::Passive;
                note.trigger_turn = false;
                if let Some(Value::Object(facts)) = note.control_payload.as_mut() {
                    facts.remove("wakeDowngraded");
                }
                // A record this build cannot project is left alone.
                let Ok(params) = crate::app_server::bus_bridge::inter_agent_params(
                    "",
                    "",
                    &snapshot.to_cutex_session_id,
                    &note,
                ) else {
                    continue;
                };
                before.push(json!({
                    "messageId": id,
                    "toCutexSessionId": snapshot.to_cutex_session_id,
                    "snapshotDeliveryMode": snapshot.delivery_mode,
                    "semanticSha256": snapshot.semantic_sha256,
                    "canonicalEnvelope": canonical,
                }));
                stored.snapshot.delivery_mode = AgentDeliveryMode::Passive.event_label().to_string();
                stored.snapshot.semantic_sha256 =
                    Some(crate::app_server::bus_bridge::inter_agent_semantic_sha256(&params));
                stored.snapshot.error = None;
                stored.canonical_envelope = Some(note);
                stored.updated_at = Utc::now().to_rfc3339();
            }
            let changed = !before.is_empty();
            Ok((before, changed))
        })
    }

    pub fn pending_v2(&self) -> anyhow::Result<Vec<PendingAgentBusMessage>> {
        self.read(|store| {
            store
                .messages
                .values()
                .filter(|stored| stored.snapshot.state == "pending")
                .map(|stored| {
                    Ok(PendingAgentBusMessage {
                        owner_cutex_session_id: stored.owner_cutex_session_id.clone(),
                        target_cutex_session_id: stored.snapshot.to_cutex_session_id.clone(),
                        canonical_envelope: stored
                            .canonical_envelope
                            .clone()
                            .context("pending v2 message lacks canonical envelope")?,
                        semantic_sha256: stored
                            .snapshot
                            .semantic_sha256
                            .clone()
                            .context("pending v2 message lacks semantic digest")?,
                        native_submission_present: stored.snapshot.a2_submission_id.is_some()
                            || stored.snapshot.a4_receipt.is_some()
                            || stored.snapshot.external_input.is_some()
                            || stored.snapshot.external_input_receipt.is_some(),
                        stale_release_authorized: stored.snapshot.stale_release_authorized,
                    })
                })
                .collect()
        })
    }

    pub fn hold_stale_pending(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
        expected_semantic_sha256: &str,
        stale_after_secs: u64,
        at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        self.mutate(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("agent-bus message disappeared before stale hold")?;
            anyhow::ensure!(
                stored.owner_cutex_session_id == owner_cutex_session_id,
                "agent-bus message owner session changed"
            );
            anyhow::ensure!(
                stored.snapshot.semantic_sha256.as_deref() == Some(expected_semantic_sha256),
                "agent-bus message semantic digest changed"
            );
            if stored.snapshot.state == "stale_held" {
                return Ok(false);
            }
            anyhow::ensure!(
                stored.snapshot.state == "pending"
                    && stored.snapshot.a2_submission_id.is_none()
                    && stored.snapshot.a4_receipt.is_none()
                    && stored.snapshot.external_input.is_none()
                    && stored.snapshot.external_input_receipt.is_none(),
                "message is no longer safe to hold before native submission"
            );
            stored.snapshot.state = "stale_held".into();
            stored.snapshot.stale_release_authorized = false;
            stored.snapshot.error = Some(json!({
                "source": "cutex",
                "code": "stale_startup_redrive_held",
                "message": "historical message held before native submission; analyze then release or discard",
                "retryable": false,
                "details": {"staleAfterSecs": stale_after_secs}
            }));
            stored.updated_at = at.to_rfc3339();
            store.version = store.version.max(7);
            Ok(true)
        })
    }

    pub fn release_stale_held(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
        expected_semantic_sha256: &str,
        at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        self.mutate(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("agent-bus message disappeared before release")?;
            anyhow::ensure!(
                stored.owner_cutex_session_id == owner_cutex_session_id,
                "agent-bus message owner session changed"
            );
            anyhow::ensure!(
                stored.snapshot.semantic_sha256.as_deref() == Some(expected_semantic_sha256),
                "agent-bus message semantic digest changed"
            );
            anyhow::ensure!(
                stored.snapshot.state == "stale_held"
                    && stored.snapshot.a2_submission_id.is_none()
                    && stored.snapshot.a4_receipt.is_none()
                    && stored.snapshot.external_input.is_none()
                    && stored.snapshot.external_input_receipt.is_none(),
                "only a pre-native stale-held message may be released"
            );
            stored.snapshot.state = "pending".into();
            stored.snapshot.stale_release_authorized = true;
            stored.snapshot.error = None;
            stored.updated_at = at.to_rfc3339();
            Ok(true)
        })
    }

    pub fn discard_stale_held(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
        expected_semantic_sha256: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !reason.trim().is_empty() && reason.len() <= 1024,
            "a bounded nonempty discard reason is required"
        );
        self.mutate(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("agent-bus message disappeared before discard")?;
            anyhow::ensure!(
                stored.owner_cutex_session_id == owner_cutex_session_id,
                "agent-bus message owner session changed"
            );
            anyhow::ensure!(
                stored.snapshot.semantic_sha256.as_deref() == Some(expected_semantic_sha256),
                "agent-bus message semantic digest changed"
            );
            anyhow::ensure!(
                stored.snapshot.state == "stale_held"
                    && stored.snapshot.a2_submission_id.is_none()
                    && stored.snapshot.a4_receipt.is_none()
                    && stored.snapshot.external_input.is_none()
                    && stored.snapshot.external_input_receipt.is_none(),
                "only a pre-native stale-held message may be discarded"
            );
            stored.snapshot.state = "discarded".into();
            stored.snapshot.stale_release_authorized = false;
            stored.snapshot.error = Some(json!({
                "source": "human",
                "code": "stale_message_discarded",
                "message": reason.trim(),
                "retryable": false,
                "details": {"semanticSha256": expected_semantic_sha256}
            }));
            stored.updated_at = at.to_rfc3339();
            store.version = store.version.max(7);
            Ok(true)
        })
    }

    /// Undelivered messages (`pending` / `stale_held`) at least
    /// `min_age_secs` old, oldest first (capability
    /// `agent-bus-stuck-messages`). Reasons cover what the repository knows;
    /// the Management route adds the live bus queue.
    pub fn stuck_messages(
        &self,
        min_age_secs: u64,
        now_epoch_secs: u64,
    ) -> anyhow::Result<Vec<StuckAgentBusMessage>> {
        self.read(|store| {
            let mut records = store
                .messages
                .values()
                .filter(|stored| matches!(stored.snapshot.state.as_str(), "pending" | "stale_held"))
                .filter_map(|stored| {
                    let created = stored_created_epoch_secs(stored);
                    let age = created
                        .filter(|created| now_epoch_secs >= *created)
                        .map(|created| now_epoch_secs - created);
                    if age.unwrap_or(u64::MAX) < min_age_secs {
                        return None;
                    }
                    Some(stuck_view(stored, created, age))
                })
                .collect::<Vec<_>>();
            records.sort_by(|a, b| {
                (a.created_at_epoch_secs, &a.message_id).cmp(&(b.created_at_epoch_secs, &b.message_id))
            });
            Ok(records)
        })
    }

    /// One message in the [`Self::stuck_messages`] form, any state.
    pub fn stuck_view_by_id(
        &self,
        message_id: &str,
        now_epoch_secs: u64,
    ) -> anyhow::Result<Option<StuckAgentBusMessage>> {
        Ok(self.get(message_id)?.map(|stored| {
            let created = stored_created_epoch_secs(&stored);
            let age = created
                .filter(|created| now_epoch_secs >= *created)
                .map(|created| now_epoch_secs - created);
            stuck_view(&stored, created, age)
        }))
    }

    /// Owner discard of one undelivered message: `pending` / `stale_held`
    /// becomes terminal and is never redriven. Without native submission
    /// evidence it becomes `discarded` (the existing tombstone); with it,
    /// `quarantined` (Cutex cannot retract what the native runtime may
    /// already hold). Error `{source: human, code: owner_message_discarded}`
    /// keeps the previous state / error. Idempotent: an already
    /// owner-discarded message answers `changed: false`. Emits
    /// `cutex/agentBus/messageFailed` once for agent-sourced messages.
    /// Task / Job / control messages need `allow_protected`.
    pub fn owner_discard(
        &self,
        message_id: &str,
        expected_semantic_sha256: Option<&str>,
        reason: &str,
        allow_protected: bool,
        at: DateTime<Utc>,
    ) -> anyhow::Result<Result<OwnerDiscardOutcome, OwnerDiscardError>> {
        let reason = reason.trim();
        if reason.is_empty() || reason.len() > 1024 {
            return Ok(Err(OwnerDiscardError::InvalidReason));
        }
        let check = |stored: &StoredAgentBusMessage| -> Result<bool, OwnerDiscardError> {
            if let Some(expected) = expected_semantic_sha256 {
                if stored.snapshot.semantic_sha256.as_deref() != Some(expected) {
                    return Err(OwnerDiscardError::DigestMismatch);
                }
            }
            if is_owner_discarded(&stored.snapshot) {
                return Ok(false);
            }
            if !matches!(stored.snapshot.state.as_str(), "pending" | "stale_held") {
                return Err(OwnerDiscardError::NotDiscardable {
                    state: stored.snapshot.state.clone(),
                });
            }
            if stored_protected_control(stored) && !allow_protected {
                return Err(OwnerDiscardError::ProtectedControl);
            }
            Ok(true)
        };
        // Typed refusals come from a read; the write below repeats every
        // check under the same lock and only then changes the record.
        let Some(stored) = self.get(message_id)? else {
            return Ok(Err(OwnerDiscardError::NotFound));
        };
        match check(&stored) {
            Err(error) => return Ok(Err(error)),
            Ok(false) => {
                return Ok(Ok(OwnerDiscardOutcome {
                    changed: false,
                    previous_state: stored
                        .snapshot
                        .error
                        .as_ref()
                        .and_then(|e| e.pointer("/details/previousState"))
                        .and_then(Value::as_str)
                        .unwrap_or(&stored.snapshot.state)
                        .to_string(),
                    owner_cutex_session_id: stored.owner_cutex_session_id.clone(),
                    to_runtime_agent_id: stored.to_runtime_agent_id.clone(),
                    snapshot: stored.snapshot,
                }))
            }
            Ok(true) => {}
        }
        let at_text = at.to_rfc3339();
        let (outcome, event) = self.mutate_checked(|store| {
            let stored = store
                .messages
                .get_mut(message_id)
                .context("agent-bus message disappeared before owner discard")?;
            match check(stored) {
                Ok(true) => {}
                // Raced with another discard / delivery: report it unchanged.
                Ok(false) | Err(_) => anyhow::bail!("agent-bus message changed during owner discard; retry"),
            }
            let previous_state = stored.snapshot.state.clone();
            let native = native_evidence(&stored.snapshot);
            let error = json!({
                "source": "human",
                "code": OWNER_DISCARD_CODE,
                "message": reason,
                "retryable": false,
                "details": {
                    "previousState": previous_state,
                    "previousError": stored.snapshot.error.clone(),
                    "semanticSha256": stored.snapshot.semantic_sha256.clone(),
                    "nativeSubmissionPresent": native,
                    "discardedAt": at_text,
                }
            });
            stored.snapshot.state = if native { "quarantined" } else { "discarded" }.into();
            stored.snapshot.stale_release_authorized = false;
            stored.snapshot.error = Some(error.clone());
            stored.updated_at = at_text.clone();
            store.version = store.version.max(7);
            let event = failed_event(stored, &error, &at_text);
            Ok((
                OwnerDiscardOutcome {
                    changed: true,
                    previous_state,
                    owner_cutex_session_id: stored.owner_cutex_session_id.clone(),
                    to_runtime_agent_id: stored.to_runtime_agent_id.clone(),
                    snapshot: stored.snapshot.clone(),
                },
                event,
            ))
        })?;
        emit_both(event)?;
        Ok(Ok(outcome))
    }

    pub fn analyze_backlog(
        &self,
        owner_cutex_session_id: &str,
        stale_after_secs: u64,
        now_epoch_secs: u64,
    ) -> anyhow::Result<Vec<AgentBusBacklogAnalysis>> {
        self.read(|store| {
            let mut records = store
                .messages
                .values()
                .filter(|stored| stored.owner_cutex_session_id == owner_cutex_session_id)
                .filter(|stored| stored.snapshot.state != "delivered")
                .map(|stored| {
                    let canonical = stored.canonical_envelope.as_ref();
                    let created = canonical.map_or(0, |message| message.created_at_epoch_secs);
                    let age = (created > 0 && now_epoch_secs >= created)
                        .then(|| now_epoch_secs.saturating_sub(created));
                    let protected = canonical.is_some_and(|message| {
                        message.sender_kind.is_task_service_system()
                            || message.sender_kind.is_job_service_system()
                            || message.control_type.is_some()
                    });
                    let native = stored.snapshot.a2_submission_id.is_some()
                        || stored.snapshot.a4_receipt.is_some()
                        || stored.snapshot.external_input.is_some()
                        || stored.snapshot.external_input_receipt.is_some();
                    let mut reasons = Vec::new();
                    let classification = if stored.snapshot.state == "discarded" {
                        reasons.push("durable discard tombstone; never redriven".into());
                        "discarded"
                    } else if native {
                        reasons.push(
                            "native submission evidence exists; Cutex cannot honestly retract it"
                                .into(),
                        );
                        "not_retractable"
                    } else if protected {
                        reasons.push(
                            "task/job/control provenance is protected from stale auto-hold".into(),
                        );
                        "protected_control"
                    } else if stored.snapshot.state == "stale_held" {
                        reasons.push("held before native submission".into());
                        reasons.push("explicit release or discard is available".into());
                        "review_held"
                    } else if stored.snapshot.state == "pending"
                        && age.is_some_and(|value| value >= stale_after_secs)
                    {
                        reasons.push(format!(
                            "age meets stale threshold of {stale_after_secs} seconds"
                        ));
                        reasons.push("will be held instead of startup redrive".into());
                        "stale_pending"
                    } else if stored.snapshot.state == "pending" {
                        reasons.push("pending message is younger than the stale threshold".into());
                        "fresh_pending"
                    } else {
                        reasons.push(format!(
                            "terminal or exceptional state: {}",
                            stored.snapshot.state
                        ));
                        "inspect_terminal"
                    };
                    AgentBusBacklogAnalysis {
                        message_id: stored.snapshot.message_id.clone(),
                        state: stored.snapshot.state.clone(),
                        semantic_sha256: stored.snapshot.semantic_sha256.clone(),
                        created_at_epoch_secs: created,
                        age_secs: age,
                        sender_kind: canonical
                            .map(|message| message.sender_kind.sender_label().to_string())
                            .unwrap_or_else(|| "unknown".into()),
                        control_type: canonical.and_then(|message| message.control_type.clone()),
                        delivery_mode: stored.snapshot.delivery_mode.clone(),
                        native_submission_present: native,
                        protected_control: protected,
                        classification: classification.into(),
                        reasons,
                        content: stored.snapshot.content.clone(),
                    }
                })
                .collect::<Vec<_>>();
            records.sort_by_key(|record| (record.created_at_epoch_secs, record.message_id.clone()));
            Ok(records)
        })
    }

    pub fn semantic_sha256(&self, message_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .get(message_id)?
            .and_then(|stored| stored.snapshot.semantic_sha256))
    }

    pub fn suppress_native_delivery(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
    ) -> anyhow::Result<bool> {
        self.read(|store| {
            let Some(stored) = store.messages.get(message_id) else {
                return Ok(false);
            };
            anyhow::ensure!(
                stored.owner_cutex_session_id == owner_cutex_session_id,
                "agent-bus message owner session changed"
            );
            // The owner discarded it (`cutex human bus-messages discard`):
            // ACK it wherever it is still queued, native evidence or not.
            if is_owner_discarded(&stored.snapshot) {
                return Ok(true);
            }
            let suppressed = matches!(stored.snapshot.state.as_str(), "stale_held" | "discarded");
            if suppressed {
                anyhow::ensure!(
                    stored.snapshot.a2_submission_id.is_none()
                        && stored.snapshot.a4_receipt.is_none()
                        && stored.snapshot.external_input.is_none()
                        && stored.snapshot.external_input_receipt.is_none(),
                    "suppressed message unexpectedly has native submission evidence"
                );
            }
            Ok(suppressed)
        })
    }

    pub fn canonical_control_type(&self, message_id: &str) -> anyhow::Result<Option<String>> {
        let stored = self
            .get(message_id)?
            .context("canonical record disappeared")?;
        Ok(stored.canonical_envelope.and_then(|m| m.control_type))
    }

    pub fn canonical_message(&self, message_id: &str) -> anyhow::Result<AgentBusMessage> {
        self.get(message_id)?
            .and_then(|stored| stored.canonical_envelope)
            .context("canonical business message unavailable; explicit review required")
    }

    /// Agent Management start messages (create / replace first messages)
    /// still in this store, by delivery state; finished ones leave with the
    /// retention sweep.
    pub fn management_start_messages(&self) -> anyhow::Result<Vec<ManagementStartMessage>> {
        use crate::agent_management::{AGENT_MANAGEMENT_START_CONTROL_TYPE, AGENT_MANAGEMENT_SYSTEM_SENDER};
        self.read(|store| {
            Ok(store
                .messages
                .iter()
                .filter_map(|(id, stored)| {
                    let canonical = stored.canonical_envelope.as_ref()?;
                    if canonical.from != AGENT_MANAGEMENT_SYSTEM_SENDER
                        || canonical.control_type.as_deref() != Some(AGENT_MANAGEMENT_START_CONTROL_TYPE)
                    {
                        return None;
                    }
                    Some(ManagementStartMessage {
                        message_id: id.clone(),
                        external_message_id: canonical.external_message_id.clone()?,
                        to_cutex_session_id: stored.snapshot.to_cutex_session_id.clone(),
                        state: stored.snapshot.state.clone(),
                        error: stored.snapshot.error.clone(),
                        native_submission_present: stored.snapshot.may_have_reached_runtime(),
                    })
                })
                .collect())
        })
    }

    pub fn snapshot_by_message_id(
        &self,
        message_id: &str,
    ) -> anyhow::Result<Option<AgentBusMessageSnapshot>> {
        Ok(self.get(message_id)?.map(|stored| stored.snapshot))
    }

    pub fn record_failed(
        &self,
        owner_cutex_session_id: &str,
        message_id: &str,
        error: Value,
        failed_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        // Both streams get the event (recipient and sender), whichever
        // side reports the failure.
        let _ = owner_cutex_session_id;
        if self.get(message_id)?.is_none() {
            return Ok(false);
        }
        let event = self.mutate(|store| {
            let stored = store.messages.get_mut(message_id).with_context(|| {
                format!("agent-bus v2 message state disappeared for {message_id}")
            })?;
            stored.snapshot.state = "failed".to_string();
            stored.snapshot.error = Some(error.clone());
            stored.updated_at = failed_at.to_rfc3339();
            Ok(failed_event(stored, &error, &failed_at.to_rfc3339()))
        })?;
        emit_both(event)?;
        Ok(true)
    }

    /// Only unresolved native deliveries need recovery polling. Avoid cloning
    /// and JSON-serializing the owner's completed message history each tick.
    pub fn pending_native_envelopes(
        &self,
        cutex_session_id: &str,
    ) -> anyhow::Result<Vec<crate::app_server::external_input::Envelope>> {
        self.read(|store| {
            Ok(store.messages.values()
                .filter(|stored| stored.owner_cutex_session_id == cutex_session_id
                    && matches!(stored.snapshot.state.as_str(), "pending" | "inbox_accepted"))
                .filter_map(|stored| stored.snapshot.external_input.clone())
                .collect())
        })
    }

    pub fn snapshot(&self, cutex_session_id: &str) -> anyhow::Result<Vec<Value>> {
        self.read(|store| {
            store
                .messages
                .values()
                .filter(|stored| stored.owner_cutex_session_id == cutex_session_id)
                .map(|stored| {
                    let mut value = serde_json::to_value(&stored.snapshot)?;
                    let peer = stored_peer_tell_fields(stored);
                    // Agent messaging v2 additions for agent-sourced messages
                    // (a cross-host tell: `name@host` sender and `remote`).
                    if stored.snapshot.from_cutex_session_id.is_some() || peer.is_some() {
                        if let Some(object) = value.as_object_mut() {
                            object.extend(stored_agent_fields(stored));
                            object.extend(peer.unwrap_or_default());
                            object.insert("queuedAt".into(), json!(sent_at(stored)));
                            if let Some(meta) = &stored.meta {
                                if let Some(turn) = &meta.context_turn_id {
                                    object.insert("contextTurnId".into(), json!(turn));
                                }
                                if let Some(at) = &meta.delivered_at {
                                    object.insert("deliveredAt".into(), json!(at));
                                }
                            }
                        }
                    } else if let (Some(object), Some(canonical)) =
                        (value.as_object_mut(), stored.canonical_envelope.as_ref())
                    {
                        object.extend(system_sender_fields(canonical));
                    }
                    Ok(value)
                })
                .collect()
        })
    }

    pub fn migrate_legacy_v1(&self) -> anyhow::Result<()> {
        // Runs on every agent registration (each refresh, every 30 s per
        // agent). A state that loads through the validated cache is v2 or
        // later; only a state the cache rejects is parsed again unchecked.
        if self.read(|_| Ok(())).is_ok() {
            return Ok(());
        }
        self.with_lock(|path| {
            let mut store = load_store_unchecked(path)?;
            if store.version != 1 {
                return Ok(());
            }
            for stored in store.messages.values_mut() {
                if stored.snapshot.state != "delivered" {
                    stored.snapshot.state = "quarantined".to_string();
                    stored.snapshot.error = Some(json!({
                        "source": "cutex",
                        "code": "legacy_v1_canonical_envelope_unavailable",
                        "message": "legacy ordinary message cannot be safely redriven",
                        "retryable": false,
                        "details": {}
                    }));
                }
            }
            store.version = 2;
            write_private_pretty_json_atomic(path, &store, "management v2 agent-bus state")
        })
    }

    /// Finished messages (delivered, inbox_accepted, quarantined, discarded,
    /// failed) last changed before `cutoff`, without a presentation still
    /// due: the retention sweep archives, then [`Self::remove_finished`]s them.
    pub fn finished_before(&self, cutoff: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Vec<FinishedAgentBusMessage>> {
        self.read(|store| {
            Ok(store
                .messages
                .values()
                .filter_map(|stored| finished(stored, cutoff))
                .collect())
        })
    }

    /// Remove finished messages (checked again under the lock). Returns how
    /// many were removed.
    pub fn remove_finished(&self, ids: &[String], cutoff: chrono::DateTime<chrono::Utc>) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.mutate(|store| {
            let before = store.messages.len();
            store
                .messages
                .retain(|id, stored| !(ids.contains(id) && finished(stored, cutoff).is_some()));
            Ok(before - store.messages.len())
        })
    }

    fn get(&self, message_id: &str) -> anyhow::Result<Option<StoredAgentBusMessage>> {
        self.read(|store| Ok(store.messages.get(message_id).cloned()))
    }

    fn mutate<T>(
        &self,
        action: impl FnOnce(&mut AgentBusMessageStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.update(|store| action(store).map(|result| (result, true)))
    }

    /// [`Self::mutate`] for an action that changes the store only after every
    /// fallible check passed: its error leaves the cached state in place.
    /// Several of these fail routinely (a polled message without a durable
    /// record, a presentation not due yet); each failure used to make the next
    /// access parse the whole 43 MB state again (Linux host, 2026-10-09).
    fn mutate_checked<T>(
        &self,
        action: impl FnOnce(&mut AgentBusMessageStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.update_inner(true, |store| action(store).map(|result| (result, true)))
    }

    /// Runs `action` on the cached store itself (taken out of the cache, not
    /// copied: a deep copy of the 46 MB state per call cost CPU and heap on
    /// Linux host, 2026-10-09). `action` returns whether it may have changed the
    /// state; only then is it serialized, and only a changed serialization is
    /// written. A failed `action` drops the (possibly half-changed) store; the
    /// next access parses the file again.
    fn update<T>(
        &self,
        action: impl FnOnce(&mut AgentBusMessageStore) -> anyhow::Result<(T, bool)>,
    ) -> anyhow::Result<T> {
        self.update_inner(false, action)
    }

    /// `unchanged_on_error`: the action never changes the store before it
    /// fails, so a failed action's store goes back into the cache.
    fn update_inner<T>(
        &self,
        unchanged_on_error: bool,
        action: impl FnOnce(&mut AgentBusMessageStore) -> anyhow::Result<(T, bool)>,
    ) -> anyhow::Result<T> {
        self.with_lock(|path| {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("management v2 agent-bus cache lock was poisoned"))?;
            let (mut store, digest, stamp) = take_store(&mut cache, path)?;
            let unchanged = |cache: &mut Option<CachedStore>, store: AgentBusMessageStore, digest: Option<(usize, u64)>| {
                if let (Some(stamp), Some(digest)) = (stamp.clone(), digest) {
                    *cache = Some(CachedStore { stamp, digest, store });
                }
            };
            let (result, changed) = match action(&mut store) {
                Ok(outcome) => outcome,
                Err(error) => {
                    if unchanged_on_error {
                        unchanged(&mut cache, store, digest);
                    }
                    return Err(error);
                }
            };
            if !changed {
                unchanged(&mut cache, store, digest);
                return Ok(result);
            }
            note_whole_state_serialization();
            let data = serde_json::to_vec_pretty(&store).with_context(|| {
                format!("Failed to serialize management v2 agent-bus state: {}", path.display())
            })?;
            // An unchanged state is not rewritten (on the Linux host a 46 MB file was
            // rewritten byte-identical every few seconds, 2026-10-09).
            let written = store_digest(&data);
            if digest == Some(written) {
                unchanged(&mut cache, store, Some(written));
                return Ok(result);
            }
            crate::config::atomic::write_private_bytes_atomic(path, &data).with_context(|| {
                format!("Failed to write management v2 agent-bus state: {}", path.display())
            })?;
            let mut index = self.admission_index.lock().map_err(|_| anyhow::anyhow!("agent-bus admission index lock poisoned"))?;
            let mut refreshed = AdmissionIndex::new(&store, Some(written));
            refreshed.stamp = crate::platform::file_stamp::FileStamp::of(path);
            refreshed.notify_changes(index.as_ref());
            *index = Some(refreshed);
            if let Some(stamp) = crate::platform::file_stamp::FileStamp::of(path) {
                *cache = Some(CachedStore { stamp, digest: written, store });
            }
            Ok(result)
        })
    }

    fn read<T>(
        &self,
        action: impl FnOnce(&AgentBusMessageStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.with_lock(|path| {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("management v2 agent-bus cache lock was poisoned"))?;
            let (store, _) = fresh_store(&mut cache, path)?;
            action(&store)
        })
    }

    fn with_lock<T>(&self, action: impl FnOnce(&Path) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let _process_guard = self
            .process_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("management v2 agent-bus lock was poisoned"))?;
        let lock_file = open_private_lock(&self.root.join(AGENT_BUS_LOCK_FILE))?;
        lock_file.lock()?;
        let result = action(&self.root.join(AGENT_BUS_STATE_FILE));
        let unlock = lock_file.unlock();
        if result.is_ok() {
            unlock?;
        }
        result
    }
}

/// Agent messaging v2 fields of an agent message event / snapshot entry:
/// `body, deliveryKind, sender, recipient` and the sender-fact flags.
pub fn agent_fields(
    delivery_mode: &str,
    body: &str,
    from: Option<&str>,
    to: &str,
    meta: Option<&AgentBusMessageMeta>,
) -> serde_json::Map<String, Value> {
    let party = |stored: Option<&AgentParty>, id: &str| {
        stored
            .filter(|party| party.id == id)
            .cloned()
            .unwrap_or_else(|| AgentParty { id: id.to_string(), name: None, runtime: None })
    };
    let mut fields = serde_json::Map::new();
    fields.insert("body".into(), json!(body));
    fields.insert(
        "deliveryKind".into(),
        json!(crate::agent_bus::delivery::delivery_kind_of_label(delivery_mode)),
    );
    if let Some(from) = from {
        fields.insert("sender".into(), json!(party(meta.and_then(|m| m.sender.as_ref()), from)));
    }
    fields.insert("recipient".into(), json!(party(meta.and_then(|m| m.recipient.as_ref()), to)));
    if let Some(facts) = meta.map(|m| &m.facts) {
        if facts.narrower_capabilities {
            fields.insert("narrowerCapabilities".into(), json!(true));
        }
        if facts.reply_only {
            fields.insert("replyOnly".into(), json!(true));
        }
        if facts.wake_downgraded {
            fields.insert("wakeDowngraded".into(), json!(true));
        }
        if let Some(turn) = &facts.sender_turn_id {
            fields.insert("senderTurnId".into(), json!(turn));
        }
    }
    fields
}

/// What the owner's queue shows of a turn-starting bus message
/// (`admission-ridealong-detail`): never more than the first line.
#[derive(Debug, PartialEq)]
struct RidealongDetail {
    kind: &'static str,
    label: String,
    from: Option<String>,
    from_host: Option<String>,
    preview: Option<String>,
    ask_id: Option<String>,
}

fn ridealong_detail(
    stored: &StoredAgentBusMessage,
    message: &AgentBusMessage,
    asks: &crate::asks::store::AskState,
) -> RidealongDetail {
    use super::admission::ridealong_preview;
    let display_source = || message.display_source.as_deref().map(str::trim).filter(|name| !name.is_empty())
        .map(str::to_string);
    if crate::agent_bus::delivery::always_after_turn(message) {
        // A legacy alarm envelope carries the old `[message from …] ` prefix.
        let legacy = format!("[message from {}] ", crate::agent_bus::alarms::ALARM_SYSTEM_SENDER);
        let preview = ridealong_preview(message.content.strip_prefix(&legacy).unwrap_or(&message.content));
        return RidealongDetail {
            kind: "alarm", label: preview.clone().unwrap_or_else(|| "Alarm".into()),
            from: None, from_host: None, preview, ask_id: None,
        };
    }
    if crate::agent_bus::peer_tell::is_peer_tell(message) {
        let facts = message.control_payload.clone()
            .and_then(|value| serde_json::from_value::<crate::agent_bus::peer_tell::PeerTellFacts>(value).ok());
        let preview = ridealong_preview(&message.content);
        return RidealongDetail {
            kind: "tell", label: preview.clone().unwrap_or_else(|| "Peer message".into()),
            from: Some(facts.as_ref().map(|facts| facts.sender_label()).unwrap_or_else(|| message.from.clone())),
            from_host: facts.map(|facts| crate::peers::wire::sanitize_name(&facts.host)),
            preview, ask_id: None,
        };
    }
    // The sending agent's display name (agent messaging v2 meta), else the
    // canonical sender id.
    let sender_name = || stored.meta.as_ref().and_then(|meta| meta.sender.as_ref())
        .filter(|party| Some(party.id.as_str()) == message.from_cutex_session_id.as_deref())
        .and_then(|party| party.name.as_deref()).map(str::trim).filter(|name| !name.is_empty())
        .map(str::to_string);
    let canonical_from = || Some(message.from.trim()).filter(|from| !from.is_empty()).map(str::to_string);
    if message.sender_kind.is_agent() && message.control_type.is_none() {
        let from = sender_name().or_else(canonical_from);
        let preview = ridealong_preview(&message.content);
        return RidealongDetail {
            kind: "tell", label: preview.clone().unwrap_or_else(|| "Agent message".into()),
            from, from_host: None, preview, ask_id: None,
        };
    }
    let control = message.control_type.as_deref();
    if control == Some(crate::agent_bus::job_completion::SCHEMA_V2) {
        let projection = crate::agent_bus::job_completion::FrozenProjection::from_message(message).ok();
        let (label, ask_id) = match &projection {
            Some(projection) => {
                let request = &projection.request;
                let status = serde_json::to_value(&request.terminal_status).ok()
                    .and_then(|value| value.as_str().map(str::to_string)).unwrap_or_else(|| "finished".into());
                let code = request.facts.exit_code.map(|code| format!(" (exit {code})")).unwrap_or_default();
                let job = crate::management::v2::summary::preview_text(&request.job_id, 40).unwrap_or_default();
                let ask_id = asks.asks.values().find(|ask| ask.remote.is_none() && ask.job(&request.job_id).is_some())
                    .map(|ask| ask.ask_id.clone());
                (format!("Job {job} {status}{code}"), ask_id)
            }
            None => ("Job finished".into(), None),
        };
        return RidealongDetail {
            kind: "bus", preview: Some(label.clone()), label,
            from: display_source().or_else(|| Some("Cutex Job Service".into())), from_host: None, ask_id,
        };
    }
    let (label, body_preview) = match control {
        Some(crate::agent_bus::model::JOB_SERVICE_COMPLETION_SCHEMA) => ("Job finished", false),
        Some(crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE) => ("Start message", true),
        Some(crate::agent_management::AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE) => ("Agent management notice", true),
        Some(_) => ("Bus delivery", false),
        None if matches!(message.sender_kind, crate::agent_bus::model::AgentMessageKind::Owner
            | crate::agent_bus::model::AgentMessageKind::User) => ("Message", true),
        None => ("System message", false),
    };
    // A control record's body is machine data: only known text bodies preview.
    let preview = body_preview.then(|| ridealong_preview(&message.content)).flatten();
    let from = sender_name().or_else(display_source).or_else(canonical_from);
    let ask_id = message.control_payload.as_ref().and_then(|payload| payload.get("askId"))
        .and_then(Value::as_str).filter(|id| asks.asks.contains_key(*id)).map(str::to_string);
    RidealongDetail { kind: "bus", label: label.into(), from, from_host: None, preview, ask_id }
}

/// Bootstrap fields of a message no agent sent (alarm, Job Service, owner …;
/// no `fromCutexSessionId`): `sender` = the canonical sender id (e.g.
/// `Cutex Alarm`, `cutex-job-service`), `senderKind` (`alarm_service`,
/// `job_service`, …), `queuedAt`, and `afterTurnOnly: true` for a message a
/// busy recipient reads only after its running turn (system alarms are never
/// read early at the next step, whatever `agent_bus.wake_busy_wait_secs`).
fn system_sender_fields(canonical: &AgentBusMessage) -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    fields.insert("sender".into(), json!({ "id": canonical.from, "name": null, "runtime": null }));
    fields.insert("senderKind".into(), json!(canonical.sender_kind.sender_label()));
    if let Some(at) = DateTime::from_timestamp(canonical.created_at_epoch_secs as i64, 0)
        .filter(|_| canonical.created_at_epoch_secs > 0)
    {
        fields.insert("queuedAt".into(), json!(at.to_rfc3339()));
    }
    if crate::agent_bus::delivery::always_after_turn(canonical) {
        fields.insert("afterTurnOnly".into(), json!(true));
    }
    fields
}

/// Display fields of a cross-host tell (`agent_bus::peer_tell`), which has no
/// local sender session: `sender` = the `name@host` label (id and name; never
/// a local session or service) and `remote: {host, peerId, senderId}`. `None`
/// for any other message, or a peer tell whose facts do not parse.
fn peer_tell_fields(from_cutex_session_id: Option<&str>, canonical: &AgentBusMessage) -> Option<serde_json::Map<String, Value>> {
    if from_cutex_session_id.is_some() || !crate::agent_bus::peer_tell::is_peer_tell(canonical) {
        return None;
    }
    let facts: crate::agent_bus::peer_tell::PeerTellFacts =
        serde_json::from_value(canonical.control_payload.clone()?).ok()?;
    let label = facts.sender_label();
    let mut fields = serde_json::Map::new();
    fields.insert("sender".into(), json!({ "id": label, "name": label, "runtime": null }));
    fields.insert("remote".into(), json!({
        "host": crate::peers::wire::sanitize_name(&facts.host),
        "peerId": facts.peer_id,
        "senderId": crate::peers::wire::sanitize_name(&facts.sender_id),
    }));
    // A cross-host wake past the receiver's hourly cap arrived as a note.
    if facts.wake_downgraded.is_some() {
        fields.insert("wakeDowngraded".into(), json!(true));
    }
    Some(fields)
}

fn stored_peer_tell_fields(stored: &StoredAgentBusMessage) -> Option<serde_json::Map<String, Value>> {
    peer_tell_fields(stored.snapshot.from_cutex_session_id.as_deref(), stored.canonical_envelope.as_ref()?)
}

fn with_agent_fields(
    mut params: Value,
    delivery_mode: &str,
    body: &str,
    from: Option<&str>,
    to: &str,
    meta: Option<&AgentBusMessageMeta>,
) -> Value {
    if let Some(object) = params.as_object_mut() {
        object.extend(agent_fields(delivery_mode, body, from, to, meta));
    }
    params
}

fn stored_agent_fields(stored: &StoredAgentBusMessage) -> serde_json::Map<String, Value> {
    agent_fields(
        &stored.snapshot.delivery_mode,
        &stored.snapshot.content,
        stored.snapshot.from_cutex_session_id.as_deref(),
        &stored.snapshot.to_cutex_session_id,
        stored.meta.as_ref(),
    )
}

/// A Management event for one or more session streams: (sessions,
/// messageId, method, params).
type BusEvent = (Vec<String>, String, &'static str, Value);

fn emit_both(event: Option<BusEvent>) -> anyhow::Result<()> {
    if let Some((sessions, message_id, method, params)) = event {
        for session in sessions {
            append_event(&session, &message_id, method, params.clone())?;
        }
    }
    Ok(())
}

fn sent_at(stored: &StoredAgentBusMessage) -> String {
    stored
        .canonical_envelope
        .as_ref()
        .filter(|c| c.created_at_epoch_secs > 0)
        .and_then(|c| DateTime::from_timestamp(c.created_at_epoch_secs as i64, 0))
        .map(|at| at.to_rfc3339())
        .unwrap_or_else(|| stored.updated_at.clone())
}

/// `messageDelivered` for an agent message, to both streams.
fn delivered_event(
    stored: &StoredAgentBusMessage,
    native_submission_id: Option<String>,
    turn_id: &str,
    recipient_runtime: &str,
    at: &str,
) -> Option<BusEvent> {
    let peer = stored_peer_tell_fields(stored);
    let from = stored.snapshot.from_cutex_session_id.as_ref();
    if from.is_none() && peer.is_none() {
        return None;
    }
    let mut params = json!({
        "messageId": stored.snapshot.message_id,
        "fromCutexSessionId": from,
        "toCutexSessionId": stored.snapshot.to_cutex_session_id,
        "fromRuntimeAgentId": stored.from_runtime_agent_id,
        "toRuntimeAgentId": stored.to_runtime_agent_id,
        "nativeSubmissionId": native_submission_id,
        "deliveredAt": at,
        "turnId": turn_id,
        "deliveryMode": stored.snapshot.delivery_mode,
        "recipientRuntime": recipient_runtime,
    });
    params.as_object_mut()?.extend(stored_agent_fields(stored));
    params.as_object_mut()?.extend(peer.unwrap_or_default());
    Some((both_streams(stored), stored.snapshot.message_id.clone(), "cutex/agentBus/messageDelivered", params))
}

/// `messageFailed` for an agent message, to both streams.
fn failed_event(stored: &StoredAgentBusMessage, error: &Value, at: &str) -> Option<BusEvent> {
    let peer = stored_peer_tell_fields(stored);
    let from = stored.snapshot.from_cutex_session_id.as_ref();
    if from.is_none() && peer.is_none() {
        return None;
    }
    let mut params = json!({
        "messageId": stored.snapshot.message_id,
        "fromCutexSessionId": from,
        "toCutexSessionId": stored.snapshot.to_cutex_session_id,
        "fromRuntimeAgentId": stored.from_runtime_agent_id,
        "toRuntimeAgentId": stored.to_runtime_agent_id,
        "error": event_error(error),
        "failedAt": at,
        "deliveryMode": stored.snapshot.delivery_mode,
    });
    params.as_object_mut()?.extend(stored_agent_fields(stored));
    params.as_object_mut()?.extend(peer.unwrap_or_default());
    Some((both_streams(stored), stored.snapshot.message_id.clone(), "cutex/agentBus/messageFailed", params))
}

/// The stored failure reason as the `cutexError` the messageFailed event
/// carries: quarantine / failure callers store `{code, message}` (some with
/// `retryable`); other keys go to `details`.
fn event_error(error: &Value) -> Value {
    let object = error.as_object().cloned().unwrap_or_default();
    let code = object
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| {
            code.starts_with(|c: char| c.is_ascii_lowercase())
                && code.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .unwrap_or("delivery_failed")
        .to_string();
    let message = match object.get("message") {
        Some(Value::String(message)) => message.clone(),
        Some(other) => other.to_string(),
        None if !error.is_object() => error.to_string(),
        None => String::new(),
    };
    let retryable = object.get("retryable").and_then(Value::as_bool).unwrap_or(false);
    let mut details = object.get("details").and_then(Value::as_object).cloned().unwrap_or_default();
    for (key, value) in &object {
        if !matches!(key.as_str(), "source" | "code" | "message" | "retryable" | "details") {
            details.insert(key.clone(), value.clone());
        }
    }
    let mut out = json!({"source": "cutex", "code": code, "message": message, "retryable": retryable});
    if !details.is_empty() {
        out["details"] = Value::Object(details);
    }
    out
}

/// The recipient and (for agent senders) the sender stream.
fn both_streams(stored: &StoredAgentBusMessage) -> Vec<String> {
    let mut sessions = vec![stored.owner_cutex_session_id.clone()];
    if let Some(from) = stored.snapshot.from_cutex_session_id.as_ref() {
        if from != &stored.owner_cutex_session_id {
            sessions.push(from.clone());
        }
    }
    sessions
}

#[cfg(test)]
thread_local! {
    /// Tests capture events here instead of the process-global repository.
    static CAPTURED_EVENTS: std::cell::RefCell<Option<Vec<(String, String, Value)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn capture_events<T>(action: impl FnOnce() -> T) -> (T, Vec<(String, String, Value)>) {
    CAPTURED_EVENTS.with(|cell| *cell.borrow_mut() = Some(Vec::new()));
    let result = action();
    let events = CAPTURED_EVENTS.with(|cell| cell.borrow_mut().take().unwrap_or_default());
    (result, events)
}

fn append_event(
    cutex_session_id: &str,
    message_id: &str,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    #[cfg(test)]
    {
        let captured = CAPTURED_EVENTS.with(|cell| {
            cell.borrow_mut().as_mut().map(|events| {
                events.push((cutex_session_id.to_string(), method.to_string(), params.clone()));
            })
        });
        if captured.is_some() {
            return Ok(());
        }
    }
    management_v2_repository()?.append(PendingEvent {
        cutex_session_id: cutex_session_id.to_string(),
        host_id: current_host_name(),
        source: EventSource::Cutex,
        schema: None,
        correlation: EventCorrelation {
            agent_bus_message_id: Some(message_id.to_string()),
            ..Default::default()
        },
        native: None,
        cutex: Some(CutexMessage {
            method: method.to_string(),
            params,
        }),
    })?;
    Ok(())
}

fn validate_session_identity(value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.contains('/') || value.contains('\\') {
        anyhow::bail!("agent-bus cutex session identity is invalid");
    }
    Ok(())
}

fn load_store(path: &Path) -> anyhow::Result<AgentBusMessageStore> {
    validate_store(load_store_unchecked(path)?)
}

/// Content digest of the serialized store (length + 64-bit hash): a mutation
/// that leaves the serialization unchanged is not written again.
fn store_digest(bytes: &[u8]) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    (bytes.len(), hasher.finish())
}

// This projection deliberately omits message bodies, rendering and retry errors.
// Its facts are identical whether read from JSON or an already validated store.
#[derive(Deserialize)]
struct AdmissionStoreProjection {
    version: u8,
    #[serde(default)]
    messages: BTreeMap<String, AdmissionMessageProjection>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AdmissionMessageProjection {
    canonical_envelope: Option<AdmissionCanonicalProjection>,
    snapshot: AdmissionSnapshotProjection,
    queued_at: Option<String>,
    #[serde(default)]
    admission_started_idle: bool,
    meta: Option<AdmissionMetaProjection>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AdmissionCanonicalProjection {
    id: String,
    #[serde(default)]
    kind: crate::agent_bus::model::AgentBusEnvelopeKind,
    from: String,
    to: String,
    from_cutex_session_id: Option<String>,
    to_cutex_session_id: Option<String>,
    #[serde(default)]
    delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode,
    trigger_turn: bool,
    created_at_epoch_secs: u64,
    #[serde(default)]
    sender_kind: crate::agent_bus::model::AgentMessageKind,
    control_type: Option<String>,
    control_payload: Option<AdmissionPayloadFingerprint>,
    external_action_id: Option<String>,
    external_message_id: Option<String>,
}

// Control payloads can embed frozen model text. Hash their JSON shape without
// retaining the text; this also detects changes to protected Job facts.
#[derive(Clone, Debug, PartialEq)]
struct AdmissionPayloadFingerprint(u64);

impl<'de> Deserialize<'de> for AdmissionPayloadFingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use std::hash::{Hash, Hasher};
        fn fingerprint(tag: u8, value: impl Hash) -> AdmissionPayloadFingerprint {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            tag.hash(&mut hash); value.hash(&mut hash);
            AdmissionPayloadFingerprint(hash.finish())
        }
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = AdmissionPayloadFingerprint;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result { formatter.write_str("JSON value") }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(fingerprint(0, ())) }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> { Ok(fingerprint(1, value)) }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> { Ok(fingerprint(2, value.to_string())) }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> { Ok(fingerprint(2, value.to_string())) }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> { Ok(fingerprint(2, value.to_string())) }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> { Ok(fingerprint(3, value)) }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(AdmissionPayloadFingerprint(value)) = seq.next_element()? { values.push(value); }
                Ok(fingerprint(4, values))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = BTreeMap::<String, u64>::new();
                while let Some((key, AdmissionPayloadFingerprint(value))) = map.next_entry()? { values.insert(key, value); }
                Ok(fingerprint(5, values))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AdmissionSnapshotProjection {
    to_cutex_session_id: String,
    state: String,
    semantic_sha256: Option<String>,
    a2_submission_id: Option<String>,
    a4_receipt: Option<AdmissionTurnProjection>,
    local_inbox_receipt: Option<Value>,
    external_input: Option<AdmissionEnvelopeProjection>,
    external_input_receipt: Option<AdmissionTurnProjection>,
    external_input_last_observed: Option<AdmissionStatusProjection>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct AdmissionEnvelopeProjection { message: AdmissionDeliveryProjection }
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct AdmissionDeliveryProjection { delivery: crate::app_server::external_input::Delivery }
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AdmissionTurnProjection { turn_id: String }
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AdmissionStatusProjection { delivery_state: crate::app_server::external_input::DeliveryState }
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AdmissionMetaProjection { context_turn_id: Option<String> }

impl From<&StoredAgentBusMessage> for AdmissionMessageProjection {
    fn from(stored: &StoredAgentBusMessage) -> Self {
        let snapshot = &stored.snapshot;
        Self {
            canonical_envelope: stored.canonical_envelope.as_ref().map(|message| AdmissionCanonicalProjection {
                id: message.id.clone(), kind: message.kind.clone(), from: message.from.clone(), to: message.to.clone(),
                from_cutex_session_id: message.from_cutex_session_id.clone(), to_cutex_session_id: message.to_cutex_session_id.clone(),
                delivery_mode: message.delivery_mode.clone(), trigger_turn: message.trigger_turn,
                created_at_epoch_secs: message.created_at_epoch_secs, sender_kind: message.sender_kind.clone(),
                control_type: message.control_type.clone(),
                control_payload: message.control_payload.as_ref().map(|payload| AdmissionPayloadFingerprint::deserialize(payload).expect("JSON value is a valid admission fingerprint")),
                external_action_id: message.external_action_id.clone(), external_message_id: message.external_message_id.clone(),
            }),
            snapshot: AdmissionSnapshotProjection {
                to_cutex_session_id: snapshot.to_cutex_session_id.clone(), state: snapshot.state.clone(),
                semantic_sha256: snapshot.semantic_sha256.clone(), a2_submission_id: snapshot.a2_submission_id.clone(),
                a4_receipt: snapshot.a4_receipt.as_ref().map(|receipt| AdmissionTurnProjection { turn_id: receipt.turn_id.clone() }),
                local_inbox_receipt: snapshot.local_inbox_receipt.clone(),
                external_input: snapshot.external_input.as_ref().map(|envelope| AdmissionEnvelopeProjection {
                    message: AdmissionDeliveryProjection { delivery: envelope.message.delivery.clone() },
                }),
                external_input_receipt: snapshot.external_input_receipt.as_ref().map(|receipt| AdmissionTurnProjection { turn_id: receipt.turn_id.clone() }),
                external_input_last_observed: snapshot.external_input_last_observed.as_ref().map(|status| AdmissionStatusProjection { delivery_state: status.delivery_state.clone() }),
            },
            queued_at: stored.queued_at.clone(), admission_started_idle: stored.admission_started_idle,
            meta: stored.meta.as_ref().map(|meta| AdmissionMetaProjection { context_turn_id: meta.context_turn_id.clone() }),
        }
    }
}

struct AdmissionIndex {
    stamp: Option<crate::platform::file_stamp::FileStamp>,
    digest: Option<(usize, u64)>,
    sessions: BTreeMap<String, Vec<String>>,
    fingerprints: BTreeMap<String, Vec<(String, AdmissionMessageProjection)>>,
    started_turns: BTreeMap<String, std::collections::BTreeSet<String>>,
}

impl AdmissionIndex {
    fn new(store: &AgentBusMessageStore, digest: Option<(usize, u64)>) -> Self {
        Self::from_projection(store.messages.iter().filter(|(_, stored)| stored.admission_started_idle
            || matches!(stored.snapshot.state.as_str(), "pending" | "inbox_accepted" | "stale_held"))
            .map(|(id, stored)| (id.clone(), AdmissionMessageProjection::from(stored))).collect(), digest)
    }

    fn from_projection(messages: BTreeMap<String, AdmissionMessageProjection>, digest: Option<(usize, u64)>) -> Self {
        let mut sessions: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut fingerprints: BTreeMap<String, Vec<(String, AdmissionMessageProjection)>> = BTreeMap::new();
        let mut started_turns: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
        for (id, stored) in messages {
            let snapshot = &stored.snapshot;
            if stored.admission_started_idle {
                let turn = snapshot.external_input_receipt.as_ref().map(|receipt| receipt.turn_id.as_str())
                    .or_else(|| snapshot.a4_receipt.as_ref().map(|receipt| receipt.turn_id.as_str()))
                    .or_else(|| snapshot.local_inbox_receipt.as_ref().and_then(|receipt| receipt.get("turnId")).and_then(Value::as_str))
                    .or_else(|| stored.meta.as_ref().and_then(|meta| meta.context_turn_id.as_deref()));
                if let Some(turn) = turn.filter(|turn| !turn.is_empty()) {
                    started_turns.entry(snapshot.to_cutex_session_id.clone()).or_default().insert(turn.into());
                }
            }
            if matches!(snapshot.state.as_str(), "pending" | "inbox_accepted" | "stale_held") {
                sessions.entry(snapshot.to_cutex_session_id.clone()).or_default().push(id.clone());
                fingerprints.entry(snapshot.to_cutex_session_id.clone()).or_default().push((id, stored));
            }
        }
        Self { stamp: None, digest, sessions, fingerprints, started_turns }
    }

    fn notify_changes(&self, previous: Option<&Self>) {
        let sessions = self.fingerprints.keys().chain(self.started_turns.keys()).chain(previous.into_iter()
            .flat_map(|old| old.fingerprints.keys().chain(old.started_turns.keys()))).collect::<std::collections::BTreeSet<_>>();
        for session in sessions {
            if previous.and_then(|old| old.fingerprints.get(session)) != self.fingerprints.get(session)
                || previous.and_then(|old| old.started_turns.get(session)) != self.started_turns.get(session) {
                super::admission::changed(session);
            }
        }
    }
}

fn bus_admission_timing(
    message: &AgentBusMessage,
    frozen: Option<&crate::app_server::external_input::Envelope>,
    now: DateTime<Utc>,
    wait: Option<std::time::Duration>,
) -> (String, Option<String>) {
    use crate::app_server::external_input::Delivery;
    if crate::agent_bus::delivery::always_after_turn(message) {
        return ("afterTurn".into(), None);
    }
    if let Some(frozen) = frozen {
        return (if frozen.message.delivery == Delivery::Soon { "nextStep" } else { "afterTurn" }.into(), None);
    }
    if message.delivery_mode == crate::agent_bus::delivery::AgentDeliveryMode::Soon {
        return ("nextStep".into(), None);
    }
    let Some(wait) = wait else { return ("afterTurn".into(), None); };
    let due = message.created_at_epoch_secs.saturating_add(wait.as_secs()).min(i64::MAX as u64) as i64;
    if now.timestamp() >= due {
        ("nextStep".into(), None)
    } else {
        ("afterTurn".into(), DateTime::<Utc>::from_timestamp(due, 0).map(|at| at.to_rfc3339()))
    }
}

/// One parsed, validated version of the state file, keyed by its identity.
struct CachedStore {
    stamp: crate::platform::file_stamp::FileStamp,
    digest: (usize, u64),
    store: AgentBusMessageStore,
}

/// Loads the state under the repository lock, reusing the cached parse while
/// the file identity is unchanged. Every writer replaces the file atomically
/// under the same cross-process lock, so another version has another stamp.
fn fresh_store<'a>(
    cache: &'a mut Option<CachedStore>,
    path: &Path,
) -> anyhow::Result<(std::borrow::Cow<'a, AgentBusMessageStore>, Option<(usize, u64)>)> {
    let stamp = crate::platform::file_stamp::FileStamp::of(path);
    let hit = matches!((cache.as_ref(), stamp.as_ref()), (Some(entry), Some(stamp)) if entry.stamp == *stamp);
    if !hit {
        *cache = None;
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            // No file yet: nothing to cache.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((std::borrow::Cow::Owned(empty_store()), None));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read management v2 agent-bus state: {}", path.display())
                })
            }
        };
        let store = validate_store(serde_json::from_slice(&bytes)?)?;
        let digest = store_digest(&bytes);
        match (stamp, crate::platform::file_stamp::FileStamp::of(path)) {
            (Some(before), Some(after)) if before == after => {
                *cache = Some(CachedStore { stamp: before, digest, store });
            }
            // Changed while read (a writer outside the lock): use, never cache.
            _ => return Ok((std::borrow::Cow::Owned(store), Some(digest))),
        }
    }
    let entry = cache.as_ref().expect("cache filled above");
    Ok((std::borrow::Cow::Borrowed(&entry.store), Some(entry.digest)))
}

/// Like [`fresh_store`], but hands out the store itself and empties the
/// cache. The stamp is `None` when the version must not be cached (no file
/// yet, or it changed while it was read).
fn take_store(
    cache: &mut Option<CachedStore>,
    path: &Path,
) -> anyhow::Result<(
    AgentBusMessageStore,
    Option<(usize, u64)>,
    Option<crate::platform::file_stamp::FileStamp>,
)> {
    let stamp = crate::platform::file_stamp::FileStamp::of(path);
    let hit = matches!((cache.as_ref(), stamp.as_ref()), (Some(entry), Some(stamp)) if entry.stamp == *stamp);
    if hit {
        let entry = cache.take().expect("cache hit above");
        return Ok((entry.store, Some(entry.digest), Some(entry.stamp)));
    }
    *cache = None;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((empty_store(), None, None));
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read management v2 agent-bus state: {}", path.display())
            })
        }
    };
    let store = validate_store(serde_json::from_slice(&bytes)?)?;
    let digest = store_digest(&bytes);
    let stamp = match (stamp, crate::platform::file_stamp::FileStamp::of(path)) {
        (Some(before), Some(after)) if before == after => Some(before),
        _ => None,
    };
    Ok((store, Some(digest), stamp))
}

fn empty_store() -> AgentBusMessageStore {
    AgentBusMessageStore {
        external_recovery_actions: BTreeMap::new(),
        version: 2,
        messages: BTreeMap::new(),
    }
}

fn validate_store(store: AgentBusMessageStore) -> anyhow::Result<AgentBusMessageStore> {
    if !matches!(store.version, 2 | 3 | 4 | 5 | 6 | 7) {
        anyhow::bail!("unsupported management v2 agent-bus state version");
    }
    if store.version < 5
        && store
            .messages
            .values()
            .any(|m| m.snapshot.presentation.is_some())
    {
        anyhow::bail!("presentation obligation requires Bus state v5");
    }
    for m in store.messages.values() {
        if let Some(canonical) = &m.canonical_envelope {
            if canonical.control_type.as_deref()
                == Some(crate::agent_bus::job_completion::SCHEMA_V2)
            {
                anyhow::ensure!(store.version >= 6, "frozen Job view requires Bus state v6");
                crate::agent_bus::job_completion::FrozenProjection::from_message(canonical)?;
            }
        }
        if let Some(envelope) = &m.snapshot.external_input {
            anyhow::ensure!(
                envelope.version != 2 || store.version >= 6,
                "structured view requires Bus state v6"
            );
            if envelope.version == 2 {
                envelope.validate()?;
            }
        }
        if let Some(p) = &m.snapshot.presentation {
            anyhow::ensure!(
                matches!(p.version, 1 | 2)
                    && m.snapshot.semantic_sha256.as_deref() == Some(&p.canonical_sha256),
                "invalid presentation obligation"
            );
            let canonical = m
                .canonical_envelope
                .as_ref()
                .context("presentation canonical absent")?;
            let expected = crate::app_server::presentation::Obligation::job_version(
                canonical,
                &p.canonical_sha256,
                p.version,
            )?;
            anyhow::ensure!(
                expected.presentation == p.presentation,
                "presentation template/canonical conflict"
            );
            if let Some(frozen) = &p.frozen {
                frozen.validate()?;
                anyhow::ensure!(
                    frozen.owner_id == m.snapshot.to_cutex_session_id
                        && frozen.presentation == p.presentation,
                    "presentation frozen conflict"
                );
            }
            if let Some(receipt) = &p.receipt {
                receipt.validate()?;
                anyhow::ensure!(
                    p.frozen.as_ref() == Some(receipt) && p.commit_generation.is_some(),
                    "presentation receipt without frozen commit"
                );
            }
        }
    }
    Ok(store)
}

fn load_store_unchecked(path: &Path) -> anyhow::Result<AgentBusMessageStore> {
    match fs::read(path) {
        Ok(bytes) => {
            let store: AgentBusMessageStore = serde_json::from_slice(&bytes)?;
            Ok(store)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(empty_store()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to read management v2 agent-bus state: {}",
                path.display()
            )
        }),
    }
}

fn validate_semantic_sha256(value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("agent-bus semanticSha256 must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn open_private_lock(path: &Path) -> anyhow::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    secure_file(path)?;
    Ok(file)
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_pending_error_and_duplicate_queue_neither_rewrite_nor_lose_writes() {
        use crate::platform::file_stamp::FileStamp;
        let root = std::env::temp_dir().join(format!("cutex-bus-pending-{}", uuid::Uuid::new_v4()));
        let first = AgentBusMessageRepository::open(&root).unwrap();
        let second = AgentBusMessageRepository::open(&root).unwrap();
        let queued = system_queued_message("pending body");
        let id = queued.message_id.clone();
        assert!(first.record_queued_isolated(queued.clone()).unwrap());
        let path = root.join(AGENT_BUS_STATE_FILE);
        // Same message again: accepted as a duplicate, nothing rewritten.
        let stamp = FileStamp::of(&path).unwrap();
        assert!(!first.record_queued_isolated(queued).unwrap());
        assert_eq!(FileStamp::of(&path).unwrap(), stamp);
        first.record_external_input_error(&id, "native pending").unwrap();
        let stamp = FileStamp::of(&path).unwrap();
        for _ in 0..3 {
            first.record_external_input_error(&id, "native pending").unwrap();
        }
        assert_eq!(FileStamp::of(&path).unwrap(), stamp, "same error must not rewrite");
        assert_eq!(
            second.get(&id).unwrap().unwrap().snapshot.error,
            Some(json!({"code":"external_input_pending","message":"native pending"}))
        );
        // A new error is written and seen by the other repository (process).
        std::thread::sleep(std::time::Duration::from_millis(20));
        first.record_external_input_error(&id, "native pending 2").unwrap();
        assert_ne!(FileStamp::of(&path).unwrap(), stamp);
        assert_eq!(
            second.get(&id).unwrap().unwrap().snapshot.error.unwrap()["message"],
            "native pending 2"
        );
        // Check-then-mutate actions that fail (a polled message without a
        // durable record, a presentation not due) keep the cached parse, and
        // the per-registration v1 migration check reads through it.
        assert!(first.get(&id).unwrap().is_some());
        assert!(first.cache.lock().unwrap().is_some());
        assert!(first.record_external_input_error("missing-message", "x").is_err());
        assert!(first.presentation_error("missing-message", "x").is_err());
        assert!(first.begin_presentation_attempt("owner", "missing-message", 0).is_err());
        assert!(first.cache.lock().unwrap().is_some(), "a checked failure keeps the cache");
        let stamp = FileStamp::of(&path).unwrap();
        first.migrate_legacy_v1().unwrap();
        assert!(first.cache.lock().unwrap().is_some());
        assert_eq!(FileStamp::of(&path).unwrap(), stamp);
        // A failed update drops the possibly half-changed store: the next
        // read comes from the file, not from the aborted change.
        let failed: anyhow::Result<()> = first.mutate(|store| {
            store.messages.get_mut(&id).unwrap().snapshot.content = "aborted".into();
            anyhow::bail!("abort")
        });
        assert!(failed.is_err());
        assert_eq!(first.get(&id).unwrap().unwrap().snapshot.content, "pending body");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unchanged_mutation_is_not_rewritten_and_other_writers_are_seen() {
        use crate::platform::file_stamp::FileStamp;
        let root = std::env::temp_dir().join(format!("cutex-bus-cache-{}", uuid::Uuid::new_v4()));
        let first = AgentBusMessageRepository::open(&root).unwrap();
        // Another process: its own repository (and cache) on the same root.
        let second = AgentBusMessageRepository::open(&root).unwrap();
        let queued = system_queued_message("original");
        let id = queued.message_id.clone();
        first.record_queued_isolated(queued).unwrap();
        let path = root.join(AGENT_BUS_STATE_FILE);
        let stamp = FileStamp::of(&path).unwrap();
        assert!(first.get(&id).unwrap().is_some());
        first.mutate(|_| Ok(())).unwrap();
        second.mutate(|store| {
            store.version = store.version.max(2);
            Ok(())
        })
        .unwrap();
        assert_eq!(FileStamp::of(&path).unwrap(), stamp, "no-op mutations must not rewrite");
        // A real change by the other writer is visible through the cache.
        std::thread::sleep(std::time::Duration::from_millis(20));
        second
            .mutate(|store| {
                store.messages.get_mut(&id).unwrap().snapshot.content = "changed".into();
                Ok(())
            })
            .unwrap();
        assert_ne!(FileStamp::of(&path).unwrap(), stamp);
        assert_eq!(first.get(&id).unwrap().unwrap().snapshot.content, "changed");
        // And this writer's own change is visible to the other one.
        first
            .mutate(|store| {
                store.messages.get_mut(&id).unwrap().snapshot.content = "again".into();
                Ok(())
            })
            .unwrap();
        assert_eq!(second.get(&id).unwrap().unwrap().snapshot.content, "again");
        assert_eq!(load_store(&path).unwrap().messages[&id].snapshot.content, "again");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_bus_freeze_replay_cas_and_native_receipt_survive_reopen() {
        use crate::app_server::external_input::*;
        let root =
            std::env::temp_dir().join(format!("cutex-external-store-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let queued = system_queued_message("original");
        let owner = queued.to_cutex_session_id.clone();
        repository.record_queued_isolated(queued).unwrap();
        let frozen = repository
            .freeze_external_input(&owner, "jsc_stable", |m| {
                let mut e = Envelope {
                    version: 1,
                    view: None,
                    owner_id: owner.clone(),
                    thread_id: "native-test".into(),
                    runtime_generation: 1,
                    message: Message {
                        id: m.id.clone(),
                        source: Source {
                            kind: SourceKind::Service,
                            id: "cutex-job-service".into(),
                        },
                        event_type: "job_completion".into(),
                        delivery: Delivery::AfterTurn,
                        text: m.content.clone(),
                    },
                    semantic_sha256: String::new(),
                };
                e.semantic_sha256 = e.digest();
                Ok(e)
            })
            .unwrap();
        let reopened = AgentBusMessageRepository::open(&root).unwrap();
        assert_eq!(reopened.pending_native_envelopes(&owner).unwrap(), vec![frozen.clone()]);
        assert!(reopened.pending_native_envelopes("other-owner").unwrap().is_empty());
        let mut permission = crate::app_server::external_recovery::RecoveryReceipt {
            action_id: "private-recovery".into(),
            retry_id: "private-retry".into(),
            review: crate::app_server::external_recovery::RecoveryReview {
                envelope: frozen.clone(),
                binding: crate::launch::stock::ExternalInputBinding {
                    version: 1,
                    owner_id: owner.clone(),
                    thread_id: frozen.thread_id.clone(),
                    runtime_generation: 1,
                    canonical_byte_limit: Default::default(),
                },
                status: Status {
                    message_id: frozen.message.id.clone(),
                    semantic_sha256: frozen.semantic_sha256.clone(),
                    delivery_state: DeliveryState::Unknown,
                    receipt: None,
                    processing: ProcessingStatus {
                        state: ProcessingState::Held,
                        attempt_id: None,
                        reason: Some(HoldReason::NoOutput),
                    },
                },
                durable_sha256: "private-spec".into(),
                authority_sha256: "private-authority".into(),
                warning: crate::app_server::external_recovery::REPEAT_WARNING.into(),
            },
            result: None,
        };
        reopened.save_recovery_action(&permission).unwrap();
        reopened.save_recovery_action(&permission).unwrap();
        assert_eq!(
            reopened.recovery_action("private-recovery").unwrap(),
            Some(permission.clone())
        );
        let mut conflict = permission.clone();
        conflict.retry_id = "different".into();
        assert!(reopened.save_recovery_action(&conflict).is_err());
        permission.result = Some(RetryResponse {
            version: 1,
            owner_id: owner.clone(),
            thread_id: frozen.thread_id.clone(),
            runtime_generation: 1,
            message_id: frozen.message.id.clone(),
            semantic_sha256: frozen.semantic_sha256.clone(),
            expected_attempt_id: None,
            retry_id: permission.retry_id.clone(),
            disposition: RetryDisposition::Released,
        });
        reopened.save_recovery_action(&permission).unwrap();
        reopened.save_recovery_action(&permission).unwrap();
        conflict = permission.clone();
        conflict.result = None;
        assert!(reopened.save_recovery_action(&conflict).is_err());
        reopened
            .mutate(|s| {
                assert_eq!(s.version, 4);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            reopened
                .freeze_external_input(&owner, "jsc_stable", |_| panic!(
                    "must not reformat frozen record"
                ))
                .unwrap(),
            frozen
        );
        let mut receipt = Receipt {
            schema: "codex.external-input-receipt.v1".into(),
            receipt_id: String::new(),
            owner_id: owner.clone(),
            thread_id: frozen.thread_id.clone(),
            message_id: frozen.message.id.clone(),
            semantic_sha256: frozen.semantic_sha256.clone(),
            response_item_id: frozen.message.id.clone(),
            turn_id: "turn-test".into(),
            ordinal: 1,
        };
        receipt.receipt_id = receipt.digest_id();
        let mut changed = frozen.clone();
        changed.message.text = "changed".into();
        changed.semantic_sha256 = changed.digest();
        assert!(reopened
            .record_external_input_delivered(&owner, "jsc_stable", &changed, &receipt, 2)
            .is_err());
        assert!(reopened
            .record_external_input_delivered("foreign", "jsc_stable", &frozen, &receipt, 2)
            .is_err());
        reopened
            .record_external_input_delivered(&owner, "jsc_stable", &frozen, &receipt, 2)
            .unwrap();
        reopened
            .record_external_input_delivered(&owner, "jsc_stable", &frozen, &receipt, 3)
            .unwrap();
        let snapshot = reopened
            .snapshot_by_message_id("jsc_stable")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.external_input_receipt, Some(receipt.clone()));
        assert!(snapshot.a4_receipt.is_none());
        assert_eq!(snapshot.state, "delivered");
        assert!(reopened.pending_native_envelopes(&owner).unwrap().is_empty(),
            "completed native deliveries must not be polled again");
        assert_eq!(snapshot.external_input_commit_generation, Some(2));
        receipt.ordinal = 2;
        receipt.receipt_id = receipt.digest_id();
        assert!(reopened
            .record_external_input_delivered(&owner, "jsc_stable", &frozen, &receipt, 3)
            .is_err());
        let raw: Value =
            serde_json::from_slice(&fs::read(root.join(AGENT_BUS_STATE_FILE)).unwrap()).unwrap();
        assert_eq!(raw["version"], 4);
        assert_eq!(
            AgentBusMessageRepository::open(&root)
                .unwrap()
                .recovery_action("private-recovery")
                .unwrap(),
            Some(permission)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_inventory_keeps_pending_inbox_and_holds_showing_only_the_first_line() {
        use crate::agent_bus::model::AgentMessageKind;
        use crate::agent_bus::delivery::AgentDeliveryMode;
        let root = std::env::temp_dir().join(format!("cutex-bus-admission-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let target = "cutex.11111111-1111-4111-8111-111111111111";
        for (id, kind, mode, state) in [
            ("tell", AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, "pending"),
            ("alarm", AgentMessageKind::AlarmSystem, AgentDeliveryMode::AfterTurn, "inbox_accepted"),
            ("bus", AgentMessageKind::JobServiceSystem, AgentDeliveryMode::AfterTurn, "stale_held"),
            ("note", AgentMessageKind::Agent, AgentDeliveryMode::Passive, "pending"),
            ("done", AgentMessageKind::Agent, AgentDeliveryMode::AfterTurn, "delivered"),
            ("removed", AgentMessageKind::TaskServiceSystem, AgentDeliveryMode::AfterTurn, "pending"),
        ] {
            let mut queued = system_queued_message("\n  first   line \nprivate message body");
            queued.message_id = id.into();
            queued.queued_at = DateTime::<Utc>::from_timestamp(1, 0).unwrap();
            queued.delivery_mode = mode.event_label().into();
            queued.canonical_envelope.id = id.into();
            queued.canonical_envelope.sender_kind = kind;
            queued.canonical_envelope.control_type = None;
            queued.canonical_envelope.delivery_mode = mode;
            repository.record_queued_isolated(queued).unwrap();
            repository.mutate(|store| {
                let snapshot = &mut store.messages.get_mut(id).unwrap().snapshot;
                snapshot.state = state.into();
                if state == "inbox_accepted" { snapshot.local_inbox_receipt = Some(json!({"durable": true})); }
                Ok(())
            }).unwrap();
        }
        let reopened = AgentBusMessageRepository::open(&root).unwrap();
        let items = reopened.pending_admission_messages(target, &crate::asks::store::AskState::default()).unwrap();
        assert_eq!(items.len(), 3);
        for kind in ["alarm", "tell", "bus"] {
            let item = items.iter().find(|item| item.id == kind).unwrap();
            assert_eq!(item.delivery_kind, kind);
            assert_eq!(item.enqueued_at, "1970-01-01T00:00:01+00:00");
            assert!(!item.label.as_ref().unwrap().contains("private"));
            assert!(item.preview.as_deref().is_none_or(|preview| !preview.contains("private")));
        }
        let by_id = |id: &str| items.iter().find(|item| item.id == id).unwrap();
        // The owner sees the first line of an alarm / tell, never the rest.
        assert_eq!(by_id("alarm").label.as_deref(), Some("first line"));
        assert_eq!(by_id("alarm").preview.as_deref(), Some("first line"));
        assert_eq!(by_id("alarm").from, None);
        assert_eq!(by_id("tell").label.as_deref(), Some("first line"));
        assert_eq!(by_id("tell").from.as_deref(), Some("cutex-job-service"));
        // A system message without a control record is machine data: no preview.
        assert_eq!(by_id("bus").preview, None);
        assert_eq!(by_id("bus").label.as_deref(), Some("System message"));
        assert_eq!(by_id("bus").from.as_deref(), Some("Cutex Job Service"));
        assert!(!items.iter().find(|item| item.id == "bus").unwrap().eligible);
        assert!(!items.iter().find(|item| item.id == "alarm").unwrap().eligible);
        assert!(items.iter().find(|item| item.id == "tell").unwrap().eligible);
        assert!(reopened.pending_admission_messages("another-session", &crate::asks::store::AskState::default()).unwrap().is_empty());
        // An owner stop holds every bus ride-along; an explicit delegation
        // pause does not.
        let mut asks = crate::asks::store::AskState::default();
        crate::asks::control::owner_pause(&mut asks, target, Utc::now());
        let paused = reopened.pending_admission_messages(target, &asks).unwrap();
        assert!(paused.iter().find(|item| item.id == "tell").unwrap().eligible);
        assert!(paused.iter().all(|item| item.held.is_none()));
        crate::asks::control::owner_stop(&mut asks, target, Utc::now());
        let stopped = reopened.pending_admission_messages(target, &asks).unwrap();
        assert!(stopped.iter().all(|item| !item.eligible && item.held.as_deref() == Some("ownerStop")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ridealong_preview_is_the_first_line_cut_on_char_boundaries() {
        use super::super::admission::ridealong_preview;
        assert_eq!(ridealong_preview(""), None);
        assert_eq!(ridealong_preview(" \n\t\n"), None);
        assert_eq!(ridealong_preview("\n  a\t b  \nsecond"), Some("a b".into()));
        let long = "界".repeat(200);
        let cut = ridealong_preview(&long).unwrap();
        assert_eq!(cut.chars().count(), super::super::admission::RIDEALONG_PREVIEW_CHARS);
        assert!(cut.ends_with('…'));
        let exact = "x".repeat(80);
        assert_eq!(ridealong_preview(&exact).as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn ridealong_detail_names_senders_and_delegations() {
        use crate::agent_bus::model::AgentMessageKind;
        let root = std::env::temp_dir().join(format!("cutex-bus-ridealong-detail-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let target = system_queued_message("x").to_cutex_session_id;
        let detail = |stored_id: &str, asks: &crate::asks::store::AskState| {
            repository.read(|store| {
                let stored = &store.messages[stored_id];
                Ok(ridealong_detail(stored, stored.canonical_envelope.as_ref().unwrap(), asks))
            }).unwrap()
        };
        let asks = crate::asks::store::AskState::default();
        // An agent tell: the sender's display name and the first line.
        let mut tell = system_queued_message("please review\nthe whole body");
        tell.message_id = "tell".into();
        tell.canonical_envelope.id = "tell".into();
        tell.canonical_envelope.from = "cutex.sender".into();
        tell.canonical_envelope.from_cutex_session_id = Some("cutex.sender".into());
        tell.from_cutex_session_id = Some("cutex.sender".into());
        tell.canonical_envelope.sender_kind = AgentMessageKind::Agent;
        tell.canonical_envelope.control_type = None;
        tell.canonical_envelope.display_source = None;
        repository.record_queued_isolated(tell).unwrap();
        repository.mutate(|store| {
            store.messages.get_mut("tell").unwrap().meta = Some(AgentBusMessageMeta {
                sender: Some(AgentParty { id: "cutex.sender".into(), name: Some("Reviewer".into()), runtime: None }),
                ..Default::default()
            });
            Ok(())
        }).unwrap();
        let got = detail("tell", &asks);
        assert_eq!((got.kind, got.label.as_str(), got.from.as_deref(), got.preview.as_deref()),
            ("tell", "please review", Some("Reviewer"), Some("please review")));
        // A cross-host tell: `name@host` and the host label.
        let mut peer = system_queued_message("see the log");
        peer.message_id = "peer".into();
        peer.canonical_envelope.id = "peer".into();
        peer.canonical_envelope.from = "worker@alpha".into();
        peer.canonical_envelope.sender_kind = AgentMessageKind::Agent;
        peer.canonical_envelope.display_source = None;
        peer.canonical_envelope.control_type = Some(crate::agent_bus::peer_tell::CONTROL_TYPE.into());
        peer.canonical_envelope.control_payload = Some(json!({"peerId":"ci-0123456789abcdef0123456789abcdef","host":"alpha","senderId":"cutex.remote","senderName":"worker","tellId":"ptl-1"}));
        repository.record_queued_isolated(peer).unwrap();
        let got = detail("peer", &asks);
        assert_eq!((got.kind, got.from.as_deref(), got.from_host.as_deref(), got.preview.as_deref()),
            ("tell", Some("worker@alpha"), Some("alpha"), Some("see the log")));
        // A legacy alarm envelope: the prefix is not part of the preview.
        let mut alarm = system_queued_message("[message from Cutex Alarm] stand-up\nmore");
        alarm.message_id = "alarm".into();
        alarm.canonical_envelope.id = "alarm".into();
        alarm.canonical_envelope.from = crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        alarm.canonical_envelope.sender_kind = AgentMessageKind::Agent;
        alarm.canonical_envelope.control_type = None;
        repository.record_queued_isolated(alarm).unwrap();
        let got = detail("alarm", &asks);
        assert_eq!((got.kind, got.label.as_str(), got.from.as_deref()), ("alarm", "stand-up", None));
        // A Job v2 completion bound to a delegation names it; the frozen JSON
        // body is never previewed.
        use crate::agent_bus::job_completion::{CompletionV2, FrozenProjection, SCHEMA_V2};
        let mut job = system_queued_message("");
        let request: CompletionV2 = serde_json::from_value(json!({
            "schema":SCHEMA_V2,"eventId":"event-1","jobId":"job-1","jobRevision":1,
            "terminalStatus":"failed","resultSha256":"a".repeat(64),"targetCutexSessionId":target,
            "facts":{"factsVersion":1,"actionId":"human-job","exitCode":2,"stdout":{"retainedBytes":0,"observedBytes":0,"truncated":false},"stderr":{"retainedBytes":0,"observedBytes":0,"truncated":false}},"outputReference":"job-output:job-1"
        })).unwrap();
        let projection = FrozenProjection::new(request).unwrap();
        job.content = serde_json::to_string(&projection).unwrap();
        job.canonical_envelope.content = job.content.clone();
        job.canonical_envelope.control_type = Some(SCHEMA_V2.into());
        job.canonical_envelope.control_payload = Some(serde_json::to_value(&projection).unwrap());
        repository.record_queued_isolated(job).unwrap();
        let mut asks = crate::asks::store::AskState::default();
        let mut request = crate::asks::store::tests::new_ask("job-ask", "build it");
        request.target_cutex_session_id = target.clone();
        let (ask, _) = asks.create_ask(request, Utc::now()).unwrap();
        let unbound = detail("jsc_stable", &asks);
        assert_eq!((unbound.kind, unbound.label.as_str(), unbound.ask_id.as_deref(), unbound.from.as_deref()),
            ("bus", "Job job-1 failed (exit 2)", None, Some("Cutex Job Service")));
        asks.asks.get_mut(&ask.ask_id).unwrap().jobs.push(crate::asks::model::JobBinding {
            job_id: "job-1".into(), source: crate::asks::model::JobSource::Auto, attempt: 1,
            bound_at: Utc::now().to_rfc3339(), action_id: None, terminal: None, cancel: None,
        });
        assert_eq!(detail("jsc_stable", &asks).ask_id.as_deref(), Some(ask.ask_id.as_str()));
        // The wire shape: camelCase, absent when unknown.
        let step = super::super::admission::AdmissionNextStep {
            id: "x".into(), delivery_kind: "tell".into(), label: Some("l".into()), enqueued_at: "t".into(),
            timing: "afterTurn".into(), next_step_at: None, held: None, ask_id: None,
            from: Some("worker@alpha".into()), from_host: Some("alpha".into()), preview: None, eligible: true,
        };
        let value = serde_json::to_value(&step).unwrap();
        assert_eq!((value["from"].as_str(), value["fromHost"].as_str()), (Some("worker@alpha"), Some("alpha")));
        assert!(value.get("preview").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_restart_requires_idle_submit_marker_and_matching_receipt() {
        let root = std::env::temp_dir().join(format!("cutex-bus-admission-restart-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let target = system_queued_message("private").to_cutex_session_id;
        for (id, mode) in [("idle", crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn),
            ("joined-owner", crate::agent_bus::delivery::AgentDeliveryMode::Soon),
            ("old-record", crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn),
            ("claude", crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn)] {
            let mut queued = system_queued_message("private");
            queued.message_id = id.into();
            queued.canonical_envelope.id = id.into();
            queued.canonical_envelope.delivery_mode = mode.clone();
            queued.delivery_mode = mode.event_label().into();
            repository.record_queued_isolated(queued).unwrap();
            repository.mutate(|store| {
                let stored = store.messages.get_mut(id).unwrap();
                stored.snapshot.state = "delivered".into();
                if id == "claude" {
                    stored.meta = Some(AgentBusMessageMeta { context_turn_id: Some("turn-claude".into()), ..Default::default() });
                } else {
                    stored.snapshot.local_inbox_receipt = Some(json!({"turnId": format!("turn-{id}")}));
                }
                Ok(())
            }).unwrap();
        }
        repository.record_admission_started_idle(&target, "idle").unwrap();
        repository.record_admission_started_idle(&target, "claude").unwrap();
        let reopened = AgentBusMessageRepository::open(&root).unwrap();
        assert!(reopened.admission_started_turn(&target, "turn-idle").unwrap());
        assert!(reopened.admission_started_turn(&target, "turn-claude").unwrap());
        assert!(!reopened.admission_started_turn(&target, "turn-joined-owner").unwrap());
        assert!(!reopened.admission_started_turn(&target, "turn-old-record").unwrap());
        assert!(!reopened.admission_started_turn(&target, "another-turn").unwrap());
        assert!(!reopened.admission_started_turn("another-session", "turn-idle").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_notes_only_include_passive_agent_input_already_in_native_inbox() {
        let root = std::env::temp_dir().join(format!("cutex-bus-admission-notes-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let mut queued = system_queued_message("private note body");
        let target = queued.to_cutex_session_id.clone();
        let id = queued.message_id.clone();
        queued.canonical_envelope.sender_kind = crate::agent_bus::model::AgentMessageKind::Agent;
        queued.canonical_envelope.control_type = None;
        queued.canonical_envelope.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
        repository.record_queued_isolated(queued).unwrap();
        assert!(repository.pending_admission_notes(&target).unwrap().is_empty());
        repository.mutate(|store| {
            store.messages.get_mut(&id).unwrap().snapshot.local_inbox_receipt = Some(json!({"durable": true}));
            Ok(())
        }).unwrap();
        let notes = repository.pending_admission_notes(&target).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].label, "Agent note");
        repository.mutate(|store| {
            store.messages.get_mut(&id).unwrap().snapshot.state = "delivered".into();
            Ok(())
        }).unwrap();
        assert!(repository.pending_admission_notes(&target).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_timing_matches_busy_wake_deadline_and_alarm_exception() {
        use std::time::Duration;
        let mut message = system_queued_message("private").canonical_envelope;
        message.created_at_epoch_secs = 100;
        let before = DateTime::<Utc>::from_timestamp(200, 0).unwrap();
        let due = DateTime::<Utc>::from_timestamp(220, 0).unwrap();
        assert_eq!(bus_admission_timing(&message, None, before, Some(Duration::from_secs(120))),
            ("afterTurn".into(), Some(due.to_rfc3339())));
        assert_eq!(bus_admission_timing(&message, None, due, Some(Duration::from_secs(120))),
            ("nextStep".into(), None));
        assert_eq!(bus_admission_timing(&message, None, before, Some(Duration::ZERO)),
            ("nextStep".into(), None));
        assert_eq!(bus_admission_timing(&message, None, due, None), ("afterTurn".into(), None));
        message.sender_kind = crate::agent_bus::model::AgentMessageKind::AlarmSystem;
        assert_eq!(bus_admission_timing(&message, None, due, Some(Duration::ZERO)),
            ("afterTurn".into(), None));
    }

    #[test]
    fn admission_index_refreshes_external_writes_and_drops_terminal_messages() {
        let root = std::env::temp_dir().join(format!("cutex-bus-admission-index-{}", uuid::Uuid::new_v4()));
        let reader = AgentBusMessageRepository::open(&root).unwrap();
        let writer = AgentBusMessageRepository::open(&root).unwrap();
        let queued = system_queued_message("private");
        let target = queued.to_cutex_session_id.clone();
        let id = queued.message_id.clone();
        let asks = crate::asks::store::AskState::default();
        assert!(reader.pending_admission_messages(&target, &asks).unwrap().is_empty());
        assert!(!reader.refresh_admission_if_changed().unwrap());
        writer.record_queued_isolated(queued).unwrap();
        assert!(reader.refresh_admission_if_changed().unwrap());
        assert_eq!(reader.pending_admission_messages(&target, &asks).unwrap().len(), 1);
        let stamp = crate::platform::file_stamp::FileStamp::of(&root.join(AGENT_BUS_STATE_FILE));
        for _ in 0..10 {
            assert!(!reader.refresh_admission_if_changed().unwrap());
            assert_eq!(reader.pending_admission_messages(&target, &asks).unwrap().len(), 1);
        }
        assert_eq!(stamp, crate::platform::file_stamp::FileStamp::of(&root.join(AGENT_BUS_STATE_FILE)));
        writer.mutate(|store| { store.messages.get_mut(&id).unwrap().snapshot.state = "delivered".into(); Ok(()) }).unwrap();
        assert!(reader.refresh_admission_if_changed().unwrap());
        assert!(!reader.refresh_admission_if_changed().unwrap());
        assert!(!writer.refresh_admission_if_changed().unwrap());
        assert!(reader.pending_admission_messages(&target, &asks).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_sparse_refresh_ignores_retry_writes_without_hydrating_bodies() {
        let root = std::env::temp_dir().join(format!("cutex-bus-sparse-{}", uuid::Uuid::new_v4()));
        let writer = AgentBusMessageRepository::open(&root).unwrap();
        let reader = AgentBusMessageRepository::open(&root).unwrap();
        let target = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut queued = system_queued_message(&"private body".repeat(100_000));
        queued.to_cutex_session_id = target.clone();
        queued.canonical_envelope.to = target.clone();
        queued.canonical_envelope.to_cutex_session_id = Some(target.clone());
        writer.record_queued_isolated(queued).unwrap();
        crate::management::v2::admission::take_change(&target);
        assert!(reader.refresh_admission_if_changed().unwrap());
        assert!(crate::management::v2::admission::take_change(&target));
        assert!(reader.cache.lock().unwrap().is_none());
        let path = root.join(AGENT_BUS_STATE_FILE);
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["messages"]["jsc_stable"]["snapshot"]["error"] = json!({"retry": "new error"});
        value["messages"]["jsc_stable"]["updatedAt"] = json!("2026-10-12T01:02:03Z");
        crate::config::atomic::write_private_pretty_json_atomic(&path, &value, "admission test state").unwrap();
        assert!(reader.refresh_admission_if_changed().unwrap());
        assert!(!crate::management::v2::admission::take_change(&target));
        assert!(reader.cache.lock().unwrap().is_none());
        assert!(!reader.refresh_admission_if_changed().unwrap());
        let full: AgentBusMessageStore = serde_json::from_value(value).unwrap();
        let expected = AdmissionIndex::new(&full, None);
        let actual = reader.admission_index.lock().unwrap();
        assert_eq!(actual.as_ref().unwrap().fingerprints, expected.fingerprints);
        assert_eq!(actual.as_ref().unwrap().started_turns, expected.started_turns);
        drop(actual);
        assert_eq!(reader.pending_admission_messages(&target, &crate::asks::store::AskState::default()).unwrap().len(), 1);
        assert!(reader.cache.lock().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_sparse_refresh_observes_scheduling_facts_and_terminal_turn_receipts() {
        let root = std::env::temp_dir().join(format!("cutex-bus-sparse-facts-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let target = format!("cutex.{}", uuid::Uuid::new_v4());
        let mut queued = system_queued_message("private");
        queued.to_cutex_session_id = target.clone();
        queued.canonical_envelope.to = target.clone();
        queued.canonical_envelope.to_cutex_session_id = Some(target.clone());
        repository.record_queued_isolated(queued).unwrap();
        let path = root.join(AGENT_BUS_STATE_FILE);
        let base: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let reader = AgentBusMessageRepository::open(&root).unwrap();
        reader.refresh_admission_if_changed().unwrap();
        let other = format!("cutex.{}", uuid::Uuid::new_v4());
        for (pointer, value) in [
            ("/snapshot/state", json!("stale_held")),
            ("/canonicalEnvelope/kind", json!("control")),
            ("/canonicalEnvelope/deliveryMode", json!("passive")),
            ("/canonicalEnvelope/senderKind", json!("alarm_system")),
            ("/canonicalEnvelope/from", json!("cutex-alarm-service")),
            ("/canonicalEnvelope/fromCutexSessionId", json!(other)),
            ("/canonicalEnvelope/toCutexSessionId", json!(other)),
            ("/canonicalEnvelope/controlType", json!("cutex.task_service.removed")),
            ("/canonicalEnvelope/controlPayload", json!({"request": {"jobId": "changed"}, "modelText": "large body".repeat(1000)})),
            ("/canonicalEnvelope/externalActionId", json!("job-changed")),
            ("/canonicalEnvelope/externalMessageId", json!("event-changed")),
            ("/canonicalEnvelope/createdAtEpochSecs", json!(1)),
            ("/queuedAt", json!("2026-10-12T01:02:03Z")),
            ("/snapshot/a2SubmissionId", json!("submitted")),
            ("/snapshot/localInboxReceipt", json!({"turnId": "native-turn"})),
            ("/snapshot/externalInput", json!({"message": {"delivery": "soon", "text": "not retained"}})),
            ("/snapshot/externalInputLastObserved", json!({"deliveryState": "pending"})),
            ("/snapshot/semanticSha256", json!("b".repeat(64))),
            ("/snapshot/toCutexSessionId", json!(other)),
        ] {
            crate::management::v2::admission::take_change(&target);
            crate::management::v2::admission::take_change(&other);
            let mut changed = base.clone();
            let message = &mut changed["messages"]["jsc_stable"];
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            message.pointer_mut(parent).unwrap().as_object_mut().unwrap().insert(key.into(), value);
            crate::config::atomic::write_private_pretty_json_atomic(&path, &changed, "admission test state").unwrap();
            assert!(reader.refresh_admission_if_changed().unwrap());
            assert!(crate::management::v2::admission::take_change(&target), "missed {pointer}");
            if pointer == "/snapshot/toCutexSessionId" { assert!(crate::management::v2::admission::take_change(&other)); }
            assert!(reader.cache.lock().unwrap().is_none());
        }
        // A terminal receipt changes currentTurn even when no pending item exists.
        let mut terminal = base;
        let message = &mut terminal["messages"]["jsc_stable"];
        message["snapshot"]["state"] = json!("delivered");
        crate::config::atomic::write_private_pretty_json_atomic(&path, &terminal, "admission test state").unwrap();
        reader.refresh_admission_if_changed().unwrap();
        crate::management::v2::admission::take_change(&target);
        terminal["messages"]["jsc_stable"]["admissionStartedIdle"] = json!(true);
        for receipt in ["externalInputReceipt", "a4Receipt", "localInboxReceipt"] {
            let mut with_receipt = terminal.clone();
            with_receipt["messages"]["jsc_stable"]["snapshot"][receipt] = json!({"turnId": receipt});
            crate::config::atomic::write_private_pretty_json_atomic(&path, &with_receipt, "admission test state").unwrap();
            reader.refresh_admission_if_changed().unwrap();
            assert!(crate::management::v2::admission::take_change(&target));
            assert!(reader.admission_index.lock().unwrap().as_ref().unwrap().started_turns[&target].contains(receipt));
        }
        terminal["messages"]["jsc_stable"]["meta"] = json!({"contextTurnId": "claude-turn"});
        crate::config::atomic::write_private_pretty_json_atomic(&path, &terminal, "admission test state").unwrap();
        reader.refresh_admission_if_changed().unwrap();
        assert!(crate::management::v2::admission::take_change(&target));
        assert!(reader.admission_index.lock().unwrap().as_ref().unwrap().started_turns[&target].contains("claude-turn"));
        fs::remove_dir_all(root).unwrap();
    }

    fn system_queued_message(content: &str) -> AgentBusQueuedMessage {
        let target = "cutex.11111111-1111-4111-8111-111111111111".to_string();
        let envelope = AgentBusMessage {
            id: "jsc_stable".to_string(),
            kind: crate::agent_bus::model::AgentBusEnvelopeKind::Message,
            from: "cutex-job-service".to_string(),
            to: target.clone(),
            from_cutex_session_id: None,
            to_cutex_session_id: Some(target.clone()),
            content: content.to_string(),
            delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn,
            trigger_turn: true,
            created_at_epoch_secs: 1,
            sender_kind: crate::agent_bus::model::AgentMessageKind::JobServiceSystem,
            display_source: Some("Cutex Job Service".to_string()),
            submit_mode: None,
            control_type: Some(crate::agent_bus::model::JOB_SERVICE_COMPLETION_SCHEMA.to_string()),
            control_payload: None,
            external_action_id: Some("job-1".to_string()),
            external_message_id: Some("event-1".to_string()),
        };
        AgentBusQueuedMessage {
            owner_cutex_session_id: target.clone(),
            message_id: envelope.id.clone(),
            from_cutex_session_id: None,
            to_cutex_session_id: target,
            from_runtime_agent_id: None,
            to_runtime_agent_id: None,
            delivery_mode: "after_turn".to_string(),
            content: content.to_string(),
            queued_at: Utc::now(),
            canonical_envelope: envelope,
            semantic_sha256: crate::role_revision::sha256_bytes(content.as_bytes())
                .as_str()
                .to_string(),
        }
    }

    fn presentation_job() -> AgentBusQueuedMessage {
        let mut q = system_queued_message("model input is not the Human summary");
        q.canonical_envelope.control_payload = Some(serde_json::json!({
            "schema":crate::agent_bus::model::JOB_SERVICE_COMPLETION_SCHEMA,
            "eventId":"event-1","jobId":"job-1","jobRevision":1,"terminalStatus":"exited",
            "resultSha256":"a".repeat(64),"targetCutexSessionId":q.to_cutex_session_id,
            "summary":"bounded result data"
        }));
        q
    }

    #[test]
    fn repeated_external_input_error_does_not_rewrite_repository() {
        let root = std::env::temp_dir().join(format!(
            "agent-bus-error-idempotence-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        repository
            .record_queued_isolated(system_queued_message("pending"))
            .unwrap();
        repository
            .record_external_input_error("jsc_stable", "temporary")
            .unwrap();
        let path = root.join(AGENT_BUS_STATE_FILE);
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        repository
            .record_external_input_error("jsc_stable", "temporary")
            .unwrap();
        assert_eq!(first, std::fs::metadata(&path).unwrap().modified().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        repository
            .record_external_input_error("jsc_stable", "changed")
            .unwrap();
        assert_ne!(first, std::fs::metadata(&path).unwrap().modified().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn job_v2_canonical_freeze_reopen_tamper_and_store_version() {
        use crate::agent_bus::job_completion::{CompletionV2, FrozenProjection, SCHEMA_V2};
        let root = std::env::temp_dir().join(format!(
            "jv{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let mut q = system_queued_message("");
        let request: CompletionV2 = serde_json::from_value(json!({
            "schema":SCHEMA_V2,"eventId":"event-1","jobId":"job-1","jobRevision":1,
            "terminalStatus":"exited","resultSha256":"a".repeat(64),"targetCutexSessionId":q.to_cutex_session_id,
            "facts":{"factsVersion":1,"actionId":"human-job", "stdout":{"retainedBytes":0,"observedBytes":0,"truncated":false},"stderr":{"retainedBytes":0,"observedBytes":0,"truncated":false}},"outputReference":"job-output:job-1"
        })).unwrap();
        let projection = FrozenProjection::new(request).unwrap();
        q.content = serde_json::to_string(&projection).unwrap();
        q.canonical_envelope.content = q.content.clone();
        q.canonical_envelope.control_type = Some(SCHEMA_V2.into());
        q.canonical_envelope.control_payload = Some(serde_json::to_value(&projection).unwrap());
        let params = crate::app_server::bus_bridge::inter_agent_params(
            "",
            &q.to_cutex_session_id,
            &q.to_cutex_session_id,
            &q.canonical_envelope,
        )
        .unwrap();
        q.semantic_sha256 = crate::app_server::bus_bridge::inter_agent_semantic_sha256(&params);
        let owner = q.to_cutex_session_id.clone();
        assert!(repository.record_queued_isolated(q.clone()).unwrap());
        assert!(!repository.record_queued_isolated(q.clone()).unwrap());
        let binding = crate::launch::stock::ExternalInputBinding {
            version: 1,
            owner_id: owner.clone(),
            thread_id: "native-test".into(),
            runtime_generation: 1,
            canonical_byte_limit: Default::default(),
        };
        let envelope = repository
            .freeze_external_input(&owner, "jsc_stable", |m| {
                crate::app_server::bus_bridge::projected_external_envelope(m, &binding)
            })
            .unwrap();
        assert_eq!(envelope.version, 2);
        assert_eq!(envelope.message.text.matches("job-1").count(), 2);
        assert!(envelope.message.text.contains("outputReference: job-output:job-1"));
        assert_eq!(envelope.view.as_ref().unwrap(), &projection.view);
        #[cfg(unix)]
        {
            // Real transport to a fake consumer; not native A4/model proof.
            use crate::app_server::client::{
                AppServerClient, AppServerClientOptions, AppServerEndpoint,
            };
            use std::os::unix::net::UnixListener;
            let path = root.join("rpc");
            assert!(path.as_os_str().len() < 104, "private socket preflight");
            let listener = UnixListener::bind(&path).unwrap();
            let expected = serde_json::to_value(&envelope).unwrap();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut ws = tungstenite::accept(stream).unwrap();
                let read =
                    |ws: &mut tungstenite::WebSocket<std::os::unix::net::UnixStream>| -> Value {
                        serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap()
                    };
                let init = read(&mut ws);
                assert_eq!(init["method"], "initialize");
                ws.send(tungstenite::Message::Text(json!({"id":init["id"],"result":{"userAgent":"private-fake-consumer","externalInputVersions":[1,2]}}).to_string().into())).unwrap();
                assert_eq!(read(&mut ws)["method"], "initialized");
                let request = read(&mut ws);
                assert_eq!(request["method"], "thread/externalInput/submit");
                assert_eq!(request["params"], expected);
                assert!(request["params"]["message"].get("view").is_none());
                assert!(request["params"]["message"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("job-output:"));
                ws.send(tungstenite::Message::Text(
                    json!({"id":request["id"],"result":{"observed":true}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            });
            let mut options =
                AppServerClientOptions::new(AppServerEndpoint::UnixSocket { socket_path: path });
            options.request_timeout = std::time::Duration::from_secs(5);
            let client = AppServerClient::connect(options).unwrap();
            assert_eq!(
                client
                    .handle()
                    .request(
                        "thread/externalInput/submit",
                        serde_json::to_value(&envelope).unwrap()
                    )
                    .unwrap()["observed"],
                true
            );
            drop(client);
            server.join().unwrap();
        }
        let reopened = AgentBusMessageRepository::open(&root).unwrap();
        assert_eq!(
            reopened
                .freeze_external_input(&owner, "jsc_stable", |_| panic!(
                    "must not reproject on recovery"
                ))
                .unwrap(),
            envelope
        );
        let mut forged = q.canonical_envelope.clone();
        forged.from_cutex_session_id = Some(owner.clone());
        let mut wrong_owner = binding.clone();
        wrong_owner.owner_id = "cutex.22222222-2222-4222-8222-222222222222".into();
        assert!(crate::app_server::bus_bridge::projected_external_envelope(
            &q.canonical_envelope,
            &wrong_owner
        )
        .is_err());
        let mut changed_projection = q.canonical_envelope.clone();
        changed_projection.control_payload.as_mut().unwrap()["modelText"] =
            "changed after acceptance".into();
        assert!(crate::app_server::bus_bridge::projected_external_envelope(
            &changed_projection,
            &binding
        )
        .is_err());
        assert!(
            crate::app_server::bus_bridge::projected_external_envelope(&forged, &binding).is_err()
        );
        for template in [
            crate::app_server::presentation::JobTemplate::Suppress,
            crate::app_server::presentation::JobTemplate::ServiceSummary,
        ] {
            let policy = crate::app_server::presentation::PrivateJobPolicy {
                version: 2,
                recipients: vec![owner.clone()],
                template: Some(template),
            };
            assert!(policy
                .obligation(&q.canonical_envelope, &q.semantic_sha256)
                .unwrap()
                .is_none());
        }
        let path = root.join(AGENT_BUS_STATE_FILE);
        let mut stored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["version"], 6);
        stored["version"] = 5.into();
        fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(reopened.snapshot_by_message_id("jsc_stable").is_err());
        fs::remove_dir_all(root).unwrap();
    }
    fn fixture_a4(repository: &AgentBusMessageRepository, owner: &str) {
        use crate::app_server::external_input::*;
        let e = repository
            .freeze_external_input(owner, "jsc_stable", |m| {
                let mut e = Envelope {
                    version: 1,
                    view: None,
                    owner_id: owner.into(),
                    thread_id: "native-test".into(),
                    runtime_generation: 1,
                    message: Message {
                        id: m.id.clone(),
                        source: Source {
                            kind: SourceKind::Service,
                            id: "cutex-job-service".into(),
                        },
                        event_type: "job_completion".into(),
                        delivery: Delivery::AfterTurn,
                        text: m.content.clone(),
                    },
                    semantic_sha256: String::new(),
                };
                e.semantic_sha256 = e.digest();
                Ok(e)
            })
            .unwrap();
        let mut r = Receipt {
            schema: "codex.external-input-receipt.v1".into(),
            receipt_id: String::new(),
            owner_id: owner.into(),
            thread_id: e.thread_id.clone(),
            message_id: e.message.id.clone(),
            semantic_sha256: e.semantic_sha256.clone(),
            response_item_id: e.message.id.clone(),
            turn_id: "fixture-turn".into(),
            ordinal: 1,
        };
        r.receipt_id = r.digest_id();
        repository
            .record_external_input_delivered(owner, "jsc_stable", &e, &r, 1)
            .unwrap();
    }
    #[test]
    fn presentation_v2_suppression_is_frozen_and_ack_independent() {
        use crate::app_server::presentation::{JobTemplate, PrivateJobPolicy};
        let root = std::env::temp_dir().join(format!("presentation-v2-{}", uuid::Uuid::new_v4()));
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        let q = presentation_job();
        let owner = q.to_cutex_session_id.clone();
        let mut policy = PrivateJobPolicy {
            version: 2,
            recipients: vec![owner.clone()],
            template: None,
        };
        assert!(repo
            .record_queued_private_job(q.clone(), Some(&policy))
            .unwrap());
        fixture_a4(&repo, &owner);
        let before = repo.snapshot_by_message_id("jsc_stable").unwrap().unwrap();
        assert_eq!(before.state, "delivered");
        assert!(before.presentation.is_none());
        policy.template = Some(JobTemplate::ServiceSummary);
        assert!(!repo.record_queued_private_job(q, Some(&policy)).unwrap());
        assert_eq!(
            repo.snapshot_by_message_id("jsc_stable").unwrap().unwrap(),
            before
        );
        assert!(repo.due_presentations(&owner, i64::MAX).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_v2_summary_preserves_exact_data_receipt_and_old_versions() {
        use crate::app_server::presentation::{JobTemplate, Obligation, PrivateJobPolicy};
        let root =
            std::env::temp_dir().join(format!("presentation-v2-summary-{}", uuid::Uuid::new_v4()));
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        let q = presentation_job();
        let owner = q.to_cutex_session_id.clone();
        let mut policy = PrivateJobPolicy {
            version: 2,
            recipients: vec![owner.clone()],
            template: Some(JobTemplate::ServiceSummary),
        };
        repo.record_queued_private_job(q.clone(), Some(&policy))
            .unwrap();
        fixture_a4(&repo, &owner);
        let frozen = repo
            .freeze_presentation(&owner, "jsc_stable", "native-test")
            .unwrap();
        assert_eq!(frozen.presentation.title, "Job summary");
        assert_eq!(frozen.presentation.body, "bounded result data");
        assert_eq!(frozen.presentation.references[0].id, "jsc_stable");
        let legacy = Obligation::job(&q.canonical_envelope, &q.semantic_sha256).unwrap();
        assert_ne!(frozen.presentation.id, legacy.presentation.id);
        assert!(legacy.presentation.body.contains("输出读取状态：未观测。"));
        let before = repo.snapshot_by_message_id("jsc_stable").unwrap().unwrap();
        assert_eq!(before.presentation.as_ref().unwrap().version, 2);
        // Old v5 writer's template predicate rejects v2, never relabels it v1.
        assert_ne!(before.presentation.as_ref().unwrap().version, 1);
        policy.template = Some(JobTemplate::Suppress);
        assert!(!repo
            .record_queued_private_job(q.clone(), Some(&policy))
            .unwrap());
        assert_eq!(
            repo.snapshot_by_message_id("jsc_stable").unwrap().unwrap(),
            before
        );
        drop(repo);
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        assert_eq!(
            repo.freeze_presentation(&owner, "jsc_stable", "native-test")
                .unwrap(),
            frozen
        );
        repo.commit_presentation(&owner, "jsc_stable", &frozen, &frozen, 1)
            .unwrap();
        repo.commit_presentation(&owner, "jsc_stable", &frozen, &frozen, 2)
            .unwrap();
        let after = repo.snapshot_by_message_id("jsc_stable").unwrap().unwrap();
        assert_eq!(after.external_input_receipt, before.external_input_receipt);
        assert_eq!(after.state, "delivered");
        assert_eq!(
            after.presentation.as_ref().unwrap().commit_generation,
            Some(1)
        );
        let mut store = load_store(&root.join(AGENT_BUS_STATE_FILE)).unwrap();
        store
            .messages
            .get_mut("jsc_stable")
            .unwrap()
            .snapshot
            .presentation
            .as_mut()
            .unwrap()
            .version = 1;
        write_private_pretty_json_atomic(
            &root.join(AGENT_BUS_STATE_FILE),
            &store,
            "fixture wrong template version",
        )
        .unwrap();
        assert!(load_store(&root.join(AGENT_BUS_STATE_FILE)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_v2_missing_summary_does_not_invent_status_or_query() {
        use crate::app_server::presentation::{JobTemplate, PrivateJobPolicy};
        let mut q = presentation_job();
        let policy = PrivateJobPolicy {
            version: 2,
            recipients: vec![q.to_cutex_session_id.clone()],
            template: Some(JobTemplate::ServiceSummary),
        };
        for summary in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!(" \n\t"),
        ] {
            q.canonical_envelope.control_payload.as_mut().unwrap()["summary"] = summary;
            assert!(policy
                .obligation(&q.canonical_envelope, &q.semantic_sha256)
                .unwrap()
                .is_none());
        }
        q.canonical_envelope.control_payload.as_mut().unwrap()["summary"] =
            serde_json::json!("  Exact summary\n第二行  ");
        assert_eq!(
            policy
                .obligation(&q.canonical_envelope, &q.semantic_sha256)
                .unwrap()
                .unwrap()
                .presentation
                .body,
            "  Exact summary\n第二行  "
        );
        q.canonical_envelope.from = "forged".into();
        assert!(policy
            .obligation(&q.canonical_envelope, &q.semantic_sha256)
            .is_err());
    }

    #[test]
    fn presentation_obligation_survives_input_ack_reopen_and_exact_receipt_cas() {
        use crate::app_server::presentation::PrivateJobPolicy;
        let root =
            std::env::temp_dir().join(format!("presentation-store-{}", uuid::Uuid::new_v4()));
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        let q = presentation_job();
        let owner = q.to_cutex_session_id.clone();
        let policy = PrivateJobPolicy {
            version: 1,
            recipients: vec![owner.clone()],
            template: None,
        };
        assert!(repo
            .record_queued_private_job(q.clone(), Some(&policy))
            .unwrap());
        assert!(repo
            .freeze_presentation(&owner, "jsc_stable", "native-test")
            .is_err());
        fixture_a4(&repo, &owner); // Synthetic A4; this test proves repository semantics only.
        let before = repo.snapshot_by_message_id("jsc_stable").unwrap().unwrap();
        assert_eq!(before.state, "delivered");
        drop(repo);
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        assert_eq!(
            repo.due_presentations(&owner, 100).unwrap(),
            vec!["jsc_stable"]
        );
        repo.begin_presentation_attempt(&owner, "jsc_stable", 100)
            .unwrap();
        assert!(repo.due_presentations(&owner, 100).unwrap().is_empty());
        let frozen = repo
            .freeze_presentation(&owner, "jsc_stable", "native-test")
            .unwrap();
        assert_ne!(frozen.presentation.body, q.content);
        assert_eq!(frozen.presentation.title, "Job 终态通知");
        assert_eq!(frozen.presentation.body, "Job: job-1\n状态：exited\n输出读取状态：未观测。\n摘要（外部数据）：bounded result data");
        assert!(!frozen.presentation.body.contains("退出码 0"));
        assert!(repo
            .freeze_presentation(&owner, "jsc_stable", "foreign-native")
            .is_err());
        // Lost append reply/local CAS: same persisted frozen fact after reopening.
        drop(repo);
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        assert_eq!(
            repo.freeze_presentation(&owner, "jsc_stable", "native-test")
                .unwrap(),
            frozen
        );
        let mut changed = frozen.clone();
        changed.presentation.body.push('!');
        assert!(repo
            .commit_presentation(&owner, "jsc_stable", &frozen, &changed, 2)
            .is_err());
        assert!(repo
            .commit_presentation("foreign", "jsc_stable", &frozen, &frozen, 2)
            .is_err());
        repo.commit_presentation(&owner, "jsc_stable", &frozen, &frozen, 2)
            .unwrap();
        repo.commit_presentation(&owner, "jsc_stable", &frozen, &frozen, 3)
            .unwrap();
        let after = repo.snapshot_by_message_id("jsc_stable").unwrap().unwrap();
        assert_eq!(after.external_input_receipt, before.external_input_receipt);
        assert_eq!(after.state, before.state);
        assert_eq!(after.presentation.unwrap().commit_generation, Some(2));
        assert!(repo.due_presentations(&owner, 200).unwrap().is_empty());
        assert!(!repo.record_queued_private_job(q, None).unwrap());
        assert_eq!(
            load_store(&root.join(AGENT_BUS_STATE_FILE))
                .unwrap()
                .version,
            5
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn presentation_no_backfill_default_none_or_downgrade() {
        use crate::app_server::presentation::PrivateJobPolicy;
        let root =
            std::env::temp_dir().join(format!("presentation-legacy-{}", uuid::Uuid::new_v4()));
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        let q = presentation_job();
        let owner = q.to_cutex_session_id.clone();
        repo.record_queued_private_job(q.clone(), None).unwrap();
        fixture_a4(&repo, &owner);
        let policy = PrivateJobPolicy {
            version: 1,
            recipients: vec![owner.clone()],
            template: None,
        };
        assert!(!repo.record_queued_private_job(q, Some(&policy)).unwrap());
        assert!(repo
            .snapshot_by_message_id("jsc_stable")
            .unwrap()
            .unwrap()
            .presentation
            .is_none());
        let mut q = presentation_job();
        q.message_id = "new-job".into();
        q.canonical_envelope.id = q.message_id.clone();
        repo.record_queued_private_job(q, Some(&policy)).unwrap();
        let path = root.join(AGENT_BUS_STATE_FILE);
        let mut state = load_store(&path).unwrap();
        assert_eq!(state.version, 5);
        // Exact old reader version predicate rejects v5, rather than dropping new fields.
        assert!(!matches!(state.version, 2 | 3 | 4));
        state.version = 4;
        write_private_pretty_json_atomic(&path, &state, "fixture downgrade").unwrap();
        assert!(load_store(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn presentation_bounded_round_robin_preserves_waiting_work_and_rejects_forged_source() {
        use crate::app_server::presentation::PrivateJobPolicy;
        let root =
            std::env::temp_dir().join(format!("presentation-schedule-{}", uuid::Uuid::new_v4()));
        let repo = AgentBusMessageRepository::open(&root).unwrap();
        let owner = presentation_job().to_cutex_session_id;
        let policy = PrivateJobPolicy {
            version: 1,
            recipients: vec![owner.clone()],
            template: None,
        };
        for n in 0..6 {
            let mut q = presentation_job();
            q.message_id = format!("job-{n}");
            q.canonical_envelope.id = q.message_id.clone();
            repo.record_queued_private_job(q, Some(&policy)).unwrap();
        }
        let first = repo.due_presentations(&owner, 100).unwrap();
        assert_eq!(first.len(), 4);
        for id in &first {
            repo.begin_presentation_attempt(&owner, id, 100).unwrap();
            repo.presentation_error(id, "recipient offline; not accepted by native")
                .unwrap();
        }
        let next = repo.due_presentations(&owner, 100).unwrap();
        assert_eq!(next.len(), 2);
        assert!(next.iter().all(|id| !first.contains(id)));
        assert!(repo.due_presentations("foreign", 100).unwrap().is_empty());
        assert_eq!(repo.due_presentations(&owner, 130).unwrap().len(), 4);
        let mut forged = presentation_job();
        forged.canonical_envelope.sender_kind = crate::agent_bus::model::AgentMessageKind::Agent;
        assert!(repo
            .record_queued_private_job(forged, Some(&policy))
            .is_err());
        assert_eq!(
            load_store(&root.join(AGENT_BUS_STATE_FILE))
                .unwrap()
                .messages
                .len(),
            6
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn system_source_persists_before_redrive_and_changed_replay_conflicts() {
        let root = std::env::temp_dir().join(format!("cutex-js3a-state-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let queued = system_queued_message("terminal result");
        repository.record_queued_isolated(queued.clone()).unwrap();
        repository.record_queued_isolated(queued).unwrap();
        let reopened = AgentBusMessageRepository::open(&root).unwrap();
        let pending = reopened.pending_v2().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0]
            .canonical_envelope
            .from_cutex_session_id
            .is_none());
        let changed = system_queued_message("changed terminal result");
        assert!(reopened.record_queued_isolated(changed).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_and_delivered_updates_one_bootstrap_message() {
        let root =
            std::env::temp_dir().join(format!("cutex-v2-agent-bus-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).expect("open state repository");
        // State behavior is exercised independently from the process-global event repository.
        repository
            .mutate(|store| {
                store.messages.insert(
                    "message-1".to_string(),
                    StoredAgentBusMessage {
                        owner_cutex_session_id: "cutex.target".to_string(),
                        from_runtime_agent_id: Some("runtime-from".to_string()),
                        to_runtime_agent_id: Some("runtime-to".to_string()),
                        canonical_envelope: None,
                        queued_at: None,
                        admission_started_idle: false,
                        snapshot: AgentBusMessageSnapshot {
                            message_id: "message-1".to_string(),
                            from_cutex_session_id: Some("cutex.source".to_string()),
                            to_cutex_session_id: "cutex.target".to_string(),
                            delivery_mode: "after_turn".to_string(),
                            content: "hello".to_string(),
                            state: "queued".to_string(),
                            stale_release_authorized: false,
                            semantic_sha256: None,
                            a2_submission_id: None,
                            a4_receipt: None,
                        local_inbox_receipt: None,
                            external_input: None,
                            external_input_receipt: None,
                            external_input_last_observed: None,
                            external_input_commit_generation: None,
                            presentation: None,
                            error: None,
                        },
                        updated_at: Utc::now().to_rfc3339(),
                        meta: None,
                    },
                );
                Ok(())
            })
            .expect("seed state");
        let snapshot = repository.snapshot("cutex.target").expect("snapshot");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0]["state"], "queued");
        // An agent message keeps its agent fields, no system-sender ones.
        assert!(snapshot[0].get("senderKind").is_none());
        fs::remove_dir_all(root).expect("remove state repository");
    }

    #[test]
    fn bootstrap_names_the_system_sender_of_a_service_message() {
        let root = std::env::temp_dir().join(format!("cutex-v2-system-sender-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let job = system_queued_message("job done");
        let target = job.owner_cutex_session_id.clone();
        repository.record_queued_isolated(job).unwrap();
        let mut alarm = system_queued_message("wake up");
        alarm.message_id = "alarm-1".into();
        alarm.canonical_envelope.id = "alarm-1".into();
        alarm.canonical_envelope.from = crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        alarm.canonical_envelope.sender_kind = crate::agent_bus::model::AgentMessageKind::AlarmSystem;
        alarm.canonical_envelope.control_type = None;
        alarm.canonical_envelope.created_at_epoch_secs = 1_791_676_830;
        repository.record_queued_isolated(alarm).unwrap();
        let snapshot = repository.snapshot(&target).unwrap();
        let by_id = |id: &str| snapshot.iter().find(|m| m["messageId"] == id).unwrap().clone();
        let job = by_id("jsc_stable");
        assert_eq!(job["sender"]["id"], "cutex-job-service");
        assert_eq!(job["senderKind"], "job_service");
        assert!(job.get("afterTurnOnly").is_none());
        assert!(job["fromCutexSessionId"].is_null());
        let alarm = by_id("alarm-1");
        assert_eq!(alarm["sender"]["id"], "Cutex Alarm");
        assert_eq!(alarm["senderKind"], "alarm_service");
        assert_eq!(alarm["afterTurnOnly"], true);
        assert_eq!(alarm["queuedAt"], "2026-10-11T00:00:30+00:00");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_v1_undelivered_message_is_durably_quarantined() {
        let root =
            std::env::temp_dir().join(format!("cutex-v1-agent-bus-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(AGENT_BUS_STATE_FILE);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "messages": {
                    "legacy-1": {
                        "ownerCutexSessionId": "cutex.target",
                        "fromRuntimeAgentId": "runtime-old",
                        "toRuntimeAgentId": "runtime-target-old",
                        "snapshot": {
                            "messageId": "legacy-1",
                            "fromCutexSessionId": "cutex.source",
                            "toCutexSessionId": "cutex.target",
                            "deliveryMode": "soon",
                            "content": "legacy",
                            "state": "queued"
                        },
                        "updatedAt": "2026-08-30T00:00:00Z"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let repository = AgentBusMessageRepository::open(&root).unwrap();
        repository.migrate_legacy_v1().unwrap();
        let snapshot = repository.snapshot("cutex.target").unwrap();
        assert_eq!(snapshot[0]["state"], "quarantined");
        assert_eq!(
            snapshot[0]["error"]["code"],
            "legacy_v1_canonical_envelope_unavailable"
        );
        let persisted: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted["version"], 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_hold_release_and_discard_are_exact_auditable_transitions() {
        let root = std::env::temp_dir().join(format!(
            "cutex-stale-backlog-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let mut queued = system_queued_message("old ordinary message");
        queued.message_id = "ordinary-old".into();
        queued.canonical_envelope.id = queued.message_id.clone();
        queued.canonical_envelope.sender_kind =
            crate::agent_bus::model::AgentMessageKind::Agent;
        queued.canonical_envelope.control_type = None;
        queued.canonical_envelope.external_action_id = None;
        queued.canonical_envelope.external_message_id = None;
        queued.semantic_sha256 = "a".repeat(64);
        let owner = queued.owner_cutex_session_id.clone();
        repository.record_queued_isolated(queued).unwrap();

        let analysis = repository.analyze_backlog(&owner, 3_600, 7_201).unwrap();
        assert_eq!(analysis.len(), 1);
        assert_eq!(analysis[0].classification, "stale_pending");
        assert!(!analysis[0].protected_control);
        assert!(!analysis[0].native_submission_present);

        assert!(repository
            .hold_stale_pending(&owner, "ordinary-old", &"a".repeat(64), 3_600, Utc::now())
            .unwrap());
        assert!(repository.pending_v2().unwrap().is_empty());
        assert!(repository.suppress_native_delivery(&owner, "ordinary-old").unwrap());
        assert_eq!(
            repository.analyze_backlog(&owner, 3_600, 7_201).unwrap()[0].classification,
            "review_held"
        );

        assert!(repository
            .release_stale_held(&owner, "ordinary-old", &"a".repeat(64), Utc::now())
            .unwrap());
        let pending = repository.pending_v2().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].stale_release_authorized);

        assert!(repository
            .hold_stale_pending(&owner, "ordinary-old", &"a".repeat(64), 3_600, Utc::now())
            .unwrap());
        assert!(repository
            .discard_stale_held(
                &owner,
                "ordinary-old",
                &"a".repeat(64),
                "obsolete after repair",
                Utc::now(),
            )
            .unwrap());
        let snapshot = repository
            .snapshot_by_message_id("ordinary-old")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.state, "discarded");
        assert_eq!(snapshot.error.unwrap()["code"], "stale_message_discarded");
        assert!(repository.pending_v2().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backlog_analysis_protects_control_and_refuses_native_frozen_discard() {
        let root = std::env::temp_dir().join(format!(
            "cutex-stale-protected-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let protected = system_queued_message("durable job completion");
        let owner = protected.owner_cutex_session_id.clone();
        repository.record_queued_isolated(protected).unwrap();
        let analysis = repository.analyze_backlog(&owner, 1, 7_201).unwrap();
        assert_eq!(analysis[0].classification, "protected_control");
        assert!(analysis[0].protected_control);

        let frozen = repository
            .freeze_external_input(&owner, "jsc_stable", |message| {
                let mut envelope = crate::app_server::external_input::Envelope {
                    version: 1,
                    owner_id: owner.clone(),
                    thread_id: "11111111-1111-4111-8111-111111111111".into(),
                    runtime_generation: 1,
                    message: crate::app_server::external_input::Message {
                        id: message.id.clone(),
                        source: crate::app_server::external_input::Source {
                            kind: crate::app_server::external_input::SourceKind::Service,
                            id: "cutex-job-service".into(),
                        },
                        event_type: "task_notification".into(),
                        delivery: crate::app_server::external_input::Delivery::AfterTurn,
                        text: message.content.clone(),
                    },
                    view: None,
                    semantic_sha256: String::new(),
                };
                envelope.semantic_sha256 = envelope.digest();
                Ok(envelope)
            })
            .unwrap();
        assert_eq!(frozen.message.id, "jsc_stable");
        assert!(repository
            .hold_stale_pending(
                &owner,
                "jsc_stable",
                crate::role_revision::sha256_bytes(b"durable job completion").as_str(),
                1,
                Utc::now(),
            )
            .is_err());
        assert_eq!(
            repository.analyze_backlog(&owner, 1, 7_201).unwrap()[0].classification,
            "not_retractable"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Input overflow, spill at delivery (r215): a message frozen over the
    /// receiver's budget before Phase 2 (r214 quarantined such messages as
    /// event_too_large) is replaced once by its overlength notice; the
    /// document is stored once and reused; a stale replace is refused.
    #[test]
    fn frozen_oversized_message_spills_at_delivery_once() {
        use crate::app_server::external_input::size::{envelope_item_bytes, ReceiverLimit};
        let root = std::env::temp_dir().join(format!("cutex-spill-at-delivery-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let content = "Assignment line with \"quotes\" and 中文\n".repeat(900);
        let queued = system_queued_message(&content);
        let owner = queued.owner_cutex_session_id.clone();
        repository.record_queued_isolated(queued).unwrap();
        let frozen = repository
            .freeze_external_input(&owner, "jsc_stable", |message| {
                let mut envelope = crate::app_server::external_input::Envelope {
                    version: 1,
                    owner_id: owner.clone(),
                    thread_id: "11111111-1111-4111-8111-111111111111".into(),
                    runtime_generation: 1,
                    message: crate::app_server::external_input::Message {
                        id: message.id.clone(),
                        source: crate::app_server::external_input::Source {
                            kind: crate::app_server::external_input::SourceKind::Service,
                            id: "cutex-alarm".into(),
                        },
                        event_type: "alarm".into(),
                        delivery: crate::app_server::external_input::Delivery::AfterTurn,
                        text: message.content.clone(),
                    },
                    view: None,
                    semantic_sha256: String::new(),
                };
                envelope.semantic_sha256 = envelope.digest();
                Ok(envelope)
            })
            .unwrap();
        assert!(envelope_item_bytes(&frozen) > 30_000);
        let message = repository.canonical_message("jsc_stable").unwrap();
        let documents_dir = root.join("documents");
        let store = || crate::documents::DocumentStore::at(&documents_dir);
        let limit = ReceiverLimit::Default;
        let spill = |envelope: &crate::app_server::external_input::Envelope| {
            crate::app_server::bus_bridge::spill_frozen_external(&repository, envelope, &message, limit, store(), false)
        };
        let spilled = spill(&frozen).unwrap().expect("spilled");
        assert!(envelope_item_bytes(&spilled) <= 9_000, "{}", envelope_item_bytes(&spilled));
        let text = &spilled.message.text;
        assert!(text.starts_with("[overlength alarm] Too long to show here in full; the complete text is stored.\nfrom: cutex-alarm (service)\n"), "{text}");
        assert!(text.contains("read full text: call the MCP tool read_overlength_content (cutex server) with id=\"doc-"), "{text}");
        assert!(text.contains("Assignment line with \"quotes\" and 中文"), "{text}");
        assert_ne!(spilled.semantic_sha256, frozen.semantic_sha256);
        // Stored as the frozen envelope: later sweeps deliver it as is.
        let again = repository.freeze_external_input(&owner, "jsc_stable", |_| panic!("already frozen")).unwrap();
        assert_eq!(again, spilled);
        assert!(spill(&again).unwrap().is_none(), "fits now");
        // One document (readers: the recipient; the sender is a service).
        let records = store().of_source("message", "jsc_stable").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].readers, vec![owner.clone()]);
        let (_, bytes) = store().read(&records[0].document_id, Some(&owner)).unwrap().unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), content);
        // A replace against a stale frozen envelope is refused; the document
        // is reused, not stored again.
        let stale = spill(&frozen).unwrap_err();
        assert!(format!("{stale:#}").contains("not replaced"), "{stale:#}");
        assert_eq!(store().of_source("message", "jsc_stable").unwrap().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    /// r240 idle CPU: a passive message the native holds for the recipient's
    /// next turn stays pending, and every bridge sweep (5 s) freezes and
    /// observes it again. Those repeats change nothing and must not serialize
    /// the whole store (47 MB, ~50 ms each on the Linux host); only a real
    /// change does.
    #[test]
    fn repeated_freeze_and_same_observation_do_not_serialize_the_store() {
        use crate::app_server::external_input as ei;
        let root = std::env::temp_dir().join(format!("cutex-sweep-serialize-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let queued = system_queued_message("passive note");
        let owner = queued.owner_cutex_session_id.clone();
        repository.record_queued_isolated(queued).unwrap();
        let make = |message: &AgentBusMessage| {
            let mut envelope = ei::Envelope {
                version: 1,
                owner_id: owner.clone(),
                thread_id: "11111111-1111-4111-8111-111111111111".into(),
                runtime_generation: 1,
                message: ei::Message {
                    id: message.id.clone(),
                    source: ei::Source { kind: ei::SourceKind::Service, id: "cutex-alarm".into() },
                    event_type: "alarm".into(),
                    delivery: ei::Delivery::Passive,
                    text: message.content.clone(),
                },
                view: None,
                semantic_sha256: String::new(),
            };
            envelope.semantic_sha256 = envelope.digest();
            Ok(envelope)
        };
        let before = whole_state_serializations_on_this_thread();
        let frozen = repository.freeze_external_input(&owner, "jsc_stable", make).unwrap();
        assert_eq!(whole_state_serializations_on_this_thread() - before, 1, "the first freeze writes");
        let pending = ei::Status {
            message_id: frozen.message.id.clone(),
            semantic_sha256: frozen.semantic_sha256.clone(),
            delivery_state: ei::DeliveryState::Pending,
            receipt: None,
            processing: ei::ProcessingStatus { state: ei::ProcessingState::None, attempt_id: None, reason: None },
        };
        repository.observe_external_input("jsc_stable", &frozen, &pending).unwrap();
        repository.record_external_input_error("jsc_stable", "native pending: Pending/None/None").unwrap();
        let settled = whole_state_serializations_on_this_thread();
        for _ in 0..3 {
            // One bridge sweep of the retained-pending message.
            let again = repository.freeze_external_input(&owner, "jsc_stable", |_| panic!("already frozen")).unwrap();
            assert_eq!(again, frozen);
            repository.observe_external_input("jsc_stable", &frozen, &pending).unwrap();
            repository.record_external_input_error("jsc_stable", "native pending: Pending/None/None").unwrap();
        }
        assert_eq!(whole_state_serializations_on_this_thread(), settled, "an unchanged sweep serializes nothing");
        // A changed observation is still recorded.
        let claimed = ei::Status {
            processing: ei::ProcessingStatus { state: ei::ProcessingState::Claimed, attempt_id: Some("a1".into()), reason: None },
            ..pending.clone()
        };
        repository.observe_external_input("jsc_stable", &frozen, &claimed).unwrap();
        assert_eq!(whole_state_serializations_on_this_thread(), settled + 1);
        assert_eq!(
            repository.snapshot_by_message_id("jsc_stable").unwrap().unwrap().external_input_last_observed,
            Some(claimed)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_global_repository_guard_rejects_ambient_or_mismatched_home() {
        assert!(validate_private_test_home("/home/user", "").is_err());
        assert!(validate_private_test_home("/home/user", "/tmp/private").is_err());
        assert!(validate_private_test_home("/", "/").is_err());
        assert!(validate_private_test_home("/tmp/private", "/tmp/private").is_ok());
    }

    /// A cross-host tell (`agent_bus::peer_tell`) has no local sender session;
    /// the recipient's owner still sees it pending (bootstrap snapshot and the
    /// live `messageQueued` event) under `name@host` with a `remote` marker, and
    /// it leaves the pending list when read in (`messageDelivered`).
    #[test]
    fn pending_peer_tell_is_visible_to_the_recipient_owner() {
        use crate::agent_bus::model::AgentMessageKind;
        let root = std::env::temp_dir().join(format!("cutex-bus-peer-tell-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let mut peer = system_queued_message("see /tmp/x on alpha");
        let to = peer.to_cutex_session_id.clone();
        peer.message_id = "ptell-abc".into();
        peer.delivery_mode = "passive".into();
        let c = &mut peer.canonical_envelope;
        c.id = "ptell-abc".into();
        c.from = "worker@alpha".into();
        c.sender_kind = AgentMessageKind::Agent;
        c.display_source = None;
        c.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
        c.trigger_turn = false;
        c.external_action_id = None;
        c.external_message_id = Some("ptell-abc".into());
        c.created_at_epoch_secs = 1_760_000_000;
        c.control_type = Some(crate::agent_bus::peer_tell::CONTROL_TYPE.into());
        c.control_payload = Some(json!({"peerId":"ci-0123456789abcdef0123456789abcdef","host":"alpha","senderId":"cutex.remote","senderName":"worker","tellId":"ptl-1"}));
        let meta = AgentBusMessageMeta {
            recipient: Some(AgentParty { id: to.clone(), name: Some("Helper".into()), runtime: Some("codex".into()) }),
            ..Default::default()
        };
        let remote = json!({"host": "alpha", "peerId": "ci-0123456789abcdef0123456789abcdef", "senderId": "cutex.remote"});
        let sender = json!({"id": "worker@alpha", "name": "worker@alpha", "runtime": null});

        let (queued, events) = capture_events(|| repository.record_queued_with_meta(peer, meta));
        assert!(queued.unwrap());
        assert_eq!(events.len(), 1, "one messageQueued to the recipient: {events:?}");
        let (stream, method, params) = &events[0];
        assert_eq!((stream.as_str(), method.as_str()), (to.as_str(), "cutex/agentBus/messageQueued"));
        assert_eq!(params["fromCutexSessionId"], Value::Null);
        assert_eq!(params["body"], "see /tmp/x on alpha");
        assert_eq!(params["deliveryKind"], "note");
        assert_eq!(params["sender"], sender);
        assert_eq!(params["remote"], remote);
        assert_eq!(params["recipient"]["name"], "Helper");
        crate::management::v2::contract_validation::validate_cutex_event_message(
            &json!({"method": method, "params": params}),
        )
        .unwrap();

        let snapshot = repository.snapshot(&to).unwrap();
        let entry = &snapshot[0];
        assert_eq!(entry["state"], "pending");
        assert_eq!(entry["body"], "see /tmp/x on alpha");
        assert_eq!(entry["deliveryKind"], "note");
        assert_eq!(entry["sender"], sender);
        assert_eq!(entry["remote"], remote);
        assert_eq!(entry["queuedAt"], "2025-10-09T08:53:20+00:00");
        assert_eq!(repository.pending_note_count(&to).unwrap(), 1);

        let (delivered, events) =
            capture_events(|| repository.record_context_injected(&to, "ptell-abc", "turn-7"));
        assert!(delivered.unwrap());
        assert_eq!(events.len(), 1, "messageDelivered to the recipient only: {events:?}");
        let (stream, method, params) = &events[0];
        assert_eq!((stream.as_str(), method.as_str()), (to.as_str(), "cutex/agentBus/messageDelivered"));
        assert_eq!(params["turnId"], "turn-7");
        assert_eq!(params["remote"], remote);
        crate::management::v2::contract_validation::validate_cutex_event_message(
            &json!({"method": method, "params": params}),
        )
        .unwrap();
        let entry = &repository.snapshot(&to).unwrap()[0];
        assert_eq!((entry["state"].as_str(), entry["contextTurnId"].as_str()), (Some("delivered"), Some("turn-7")));
        assert_eq!(repository.pending_note_count(&to).unwrap(), 0);
    }

    /// A cross-host wake (`cutex.peer.tell.v2`, after_turn) shows as
    /// deliveryKind "wake" with the same `remote`; it is not a pending note.
    /// A wake past the receiver's hourly cap is a note with wakeDowngraded.
    #[test]
    fn peer_wake_shows_as_a_wake_and_a_downgraded_one_as_a_note() {
        use crate::agent_bus::model::AgentMessageKind;
        let root = std::env::temp_dir().join(format!("cutex-bus-peer-wake-{}", uuid::Uuid::new_v4()));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let facts = json!({"peerId":"ci-0123456789abcdef0123456789abcdef","host":"alpha","senderId":"cutex.remote","senderName":"worker","tellId":"ptl-1"});
        let peer = |id: &str, wake: bool, facts: Value| {
            let mut peer = system_queued_message("look at the build");
            peer.message_id = id.into();
            peer.delivery_mode = if wake { "after_turn" } else { "passive" }.into();
            let c = &mut peer.canonical_envelope;
            c.id = id.into();
            c.from = "worker@alpha".into();
            c.sender_kind = AgentMessageKind::Agent;
            c.display_source = None;
            c.delivery_mode = if wake {
                crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
            } else {
                crate::agent_bus::delivery::AgentDeliveryMode::Passive
            };
            c.trigger_turn = wake;
            c.external_action_id = None;
            c.external_message_id = Some(id.into());
            c.control_type = Some(if wake { crate::agent_bus::peer_tell::CONTROL_TYPE_V2 } else { crate::agent_bus::peer_tell::CONTROL_TYPE }.into());
            c.control_payload = Some(facts);
            peer
        };
        let to = peer("x", true, facts.clone()).to_cutex_session_id;
        let (queued, events) = capture_events(|| repository.record_queued_with_meta(peer("ptell-wake", true, facts.clone()), AgentBusMessageMeta::default()));
        assert!(queued.unwrap());
        let (_, method, params) = &events[0];
        assert_eq!(params["deliveryKind"], "wake");
        assert_eq!(params["remote"]["host"], "alpha");
        assert_eq!(params["sender"]["id"], "worker@alpha");
        assert!(params.get("wakeDowngraded").is_none());
        crate::management::v2::contract_validation::validate_cutex_event_message(&json!({"method": method, "params": params})).unwrap();
        assert_eq!(repository.pending_note_count(&to).unwrap(), 0, "a wake is not a pending note");

        let mut downgraded_facts = facts;
        downgraded_facts["wakeDowngraded"] = json!("rate_limited");
        let (queued, events) = capture_events(|| repository.record_queued_with_meta(peer("ptell-note", false, downgraded_facts), AgentBusMessageMeta::default()));
        assert!(queued.unwrap());
        let (_, method, params) = &events[0];
        assert_eq!(params["deliveryKind"], "note");
        assert_eq!(params["wakeDowngraded"], true);
        crate::management::v2::contract_validation::validate_cutex_event_message(&json!({"method": method, "params": params})).unwrap();
        let snapshot = repository.snapshot(&to).unwrap();
        let kinds: std::collections::BTreeMap<_, _> = snapshot.iter()
            .map(|entry| (entry["messageId"].as_str().unwrap().to_string(), entry["deliveryKind"].clone())).collect();
        assert_eq!(kinds["ptell-wake"], "wake");
        assert_eq!(kinds["ptell-note"], "note");
        assert_eq!(repository.pending_note_count(&to).unwrap(), 1);
    }
}

static WHOLE_STATE_SERIALIZATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static THREAD_WHOLE_STATE_SERIALIZATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn note_whole_state_serialization() {
    WHOLE_STATE_SERIALIZATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    THREAD_WHOLE_STATE_SERIALIZATIONS.with(|count| count.set(count.get() + 1));
}

/// Whole agent-bus state serializations on the calling thread (tests run in
/// parallel, so assertions use this instead of the process-wide count).
#[cfg(test)]
pub(crate) fn whole_state_serializations_on_this_thread() -> u64 {
    THREAD_WHOLE_STATE_SERIALIZATIONS.with(|count| count.get())
}

/// How many times this process serialized the whole agent-bus state (to
/// write it, or to learn a write changed nothing). A cheap diagnostic counter
/// for performance tests: a repeated delivery sweep must not serialize.
pub fn whole_state_serialization_count() -> u64 {
    WHOLE_STATE_SERIALIZATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// A finished agent bus message past retention (archive input).
#[derive(Debug, Clone)]
pub struct FinishedAgentBusMessage {
    pub message_id: String,
    pub from_cutex_session_id: Option<String>,
    pub to_cutex_session_id: String,
    pub state: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// The stored record as JSON (snapshot, canonical envelope, display facts).
    pub record: Value,
}

const FINISHED_MESSAGE_STATES: &[&str] = &["delivered", "inbox_accepted", "quarantined", "discarded", "failed"];

fn finished(stored: &StoredAgentBusMessage, cutoff: chrono::DateTime<chrono::Utc>) -> Option<FinishedAgentBusMessage> {
    if !FINISHED_MESSAGE_STATES.contains(&stored.snapshot.state.as_str()) {
        return None;
    }
    if stored.snapshot.presentation.as_ref().is_some_and(|presentation| presentation.receipt.is_none()) {
        return None;
    }
    let updated_at = chrono::DateTime::parse_from_rfc3339(&stored.updated_at).ok()?.with_timezone(&chrono::Utc);
    (updated_at < cutoff).then(|| FinishedAgentBusMessage {
        message_id: stored.snapshot.message_id.clone(),
        from_cutex_session_id: stored.snapshot.from_cutex_session_id.clone(),
        to_cutex_session_id: stored.snapshot.to_cutex_session_id.clone(),
        state: stored.snapshot.state.clone(),
        updated_at,
        record: serde_json::to_value(stored).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod owner_discard_tests {
    use super::*;
    use crate::agent_bus::queue::QueuedAgentBusMessage;
    use crate::management::v2::agent_bus_stuck::{self, DiscardRequest, LiveBus};
    use std::cell::RefCell;

    const TO: &str = "cutex.11111111-1111-4111-8111-111111111111";
    const FROM: &str = "cutex.22222222-2222-4222-8222-222222222222";
    const NOW: u64 = 1_000_000;

    fn repo(name: &str) -> (AgentBusMessageRepository, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "cutex-owner-discard-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        (AgentBusMessageRepository::open(&root).unwrap(), root)
    }

    fn message(
        id: &str,
        content: &str,
        created: u64,
        kind: crate::agent_bus::model::AgentMessageKind,
        control: Option<&str>,
    ) -> AgentBusQueuedMessage {
        let agent = kind == crate::agent_bus::model::AgentMessageKind::Agent;
        let envelope = AgentBusMessage {
            id: id.to_string(),
            kind: crate::agent_bus::model::AgentBusEnvelopeKind::Message,
            from: if agent {
                "sender-agent".into()
            } else {
                "cutex-job-service".into()
            },
            to: TO.to_string(),
            from_cutex_session_id: agent.then(|| FROM.to_string()),
            to_cutex_session_id: Some(TO.to_string()),
            content: content.to_string(),
            delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn,
            trigger_turn: true,
            created_at_epoch_secs: created,
            sender_kind: kind,
            display_source: None,
            submit_mode: None,
            control_type: control.map(str::to_string),
            control_payload: None,
            external_action_id: None,
            external_message_id: None,
        };
        AgentBusQueuedMessage {
            owner_cutex_session_id: TO.to_string(),
            message_id: id.to_string(),
            from_cutex_session_id: agent.then(|| FROM.to_string()),
            to_cutex_session_id: TO.to_string(),
            from_runtime_agent_id: agent.then(|| "rt-from".to_string()),
            to_runtime_agent_id: Some("rt-to".to_string()),
            delivery_mode: "after_turn".to_string(),
            content: content.to_string(),
            queued_at: Utc::now(),
            canonical_envelope: envelope,
            semantic_sha256: crate::role_revision::sha256_bytes(content.as_bytes())
                .as_str()
                .to_string(),
        }
    }

    fn agent_message(id: &str, created: u64) -> AgentBusQueuedMessage {
        message(
            id,
            &format!("content of {id}"),
            created,
            crate::agent_bus::model::AgentMessageKind::Agent,
            None,
        )
    }

    fn freeze(repository: &AgentBusMessageRepository, id: &str) {
        repository
            .freeze_external_input(TO, id, |message| {
                let mut envelope = crate::app_server::external_input::Envelope {
                    version: 1,
                    owner_id: TO.to_string(),
                    thread_id: "11111111-1111-4111-8111-111111111111".into(),
                    runtime_generation: 1,
                    message: crate::app_server::external_input::Message {
                        id: message.id.clone(),
                        source: crate::app_server::external_input::Source {
                            kind: crate::app_server::external_input::SourceKind::Service,
                            id: "cutex-job-service".into(),
                        },
                        event_type: "task_notification".into(),
                        delivery: crate::app_server::external_input::Delivery::AfterTurn,
                        text: message.content.clone(),
                    },
                    view: None,
                    semantic_sha256: String::new(),
                };
                envelope.semantic_sha256 = envelope.digest();
                Ok(envelope)
            })
            .unwrap();
    }

    #[test]
    fn stuck_list_and_owner_discard_are_exact_idempotent_and_suppress_delivery() {
        let (repository, root) = repo("repo");
        repository.record_queued_isolated(agent_message("old", 10)).unwrap();
        repository
            .record_queued_isolated(agent_message("fresh", NOW - 60))
            .unwrap();
        let job = message(
            "job-1",
            "job done",
            20,
            crate::agent_bus::model::AgentMessageKind::JobServiceSystem,
            Some(crate::agent_bus::model::JOB_SERVICE_COMPLETION_SCHEMA),
        );
        repository.record_queued_isolated(job).unwrap();
        freeze(&repository, "job-1");

        let stuck = repository.stuck_messages(3600, NOW).unwrap();
        assert_eq!(
            stuck.iter().map(|m| m.message_id.as_str()).collect::<Vec<_>>(),
            vec!["old", "job-1"]
        );
        assert_eq!(stuck[0].age_secs, Some(NOW - 10));
        assert!(!stuck[0].native_submission_present && !stuck[0].protected_control);
        assert!(stuck[1].native_submission_present && stuck[1].protected_control);
        assert!(stuck[1]
            .reasons
            .iter()
            .any(|r| r.contains("frozen for the native runtime")));
        assert_eq!(repository.stuck_messages(0, NOW).unwrap().len(), 3);

        let at = Utc::now();
        assert_eq!(
            repository.owner_discard("absent", None, "x", false, at).unwrap().unwrap_err(),
            OwnerDiscardError::NotFound
        );
        assert_eq!(
            repository.owner_discard("old", None, "  ", false, at).unwrap().unwrap_err(),
            OwnerDiscardError::InvalidReason
        );
        assert_eq!(
            repository
                .owner_discard("old", Some(&"0".repeat(64)), "obsolete", false, at)
                .unwrap()
                .unwrap_err(),
            OwnerDiscardError::DigestMismatch
        );
        let sha = repository.semantic_sha256("old").unwrap().unwrap();
        let (outcome, events) =
            capture_events(|| repository.owner_discard("old", Some(&sha), "obsolete", false, at));
        let outcome = outcome.unwrap().unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.previous_state, "pending");
        assert_eq!(outcome.snapshot.state, "discarded");
        let error = outcome.snapshot.error.clone().unwrap();
        assert_eq!(error["code"], OWNER_DISCARD_CODE);
        assert_eq!(error["source"], "human");
        assert_eq!(error["details"]["previousState"], "pending");
        assert_eq!(events.len(), 2, "messageFailed to recipient and sender: {events:?}");
        assert!(events.iter().all(|(_, method, params)| method
            == "cutex/agentBus/messageFailed"
            && params["error"]["code"] == OWNER_DISCARD_CODE));
        assert!(repository
            .pending_v2()
            .unwrap()
            .iter()
            .all(|m| m.canonical_envelope.id != "old"));
        assert!(repository.suppress_native_delivery(TO, "old").unwrap());
        // Idempotent: no change, no second event.
        let (again, events) =
            capture_events(|| repository.owner_discard("old", None, "other reason", false, at));
        let again = again.unwrap().unwrap();
        assert!(!again.changed);
        assert_eq!(again.previous_state, "pending");
        assert!(events.is_empty());

        // Protected + native evidence: needs allowProtected, ends quarantined,
        // and the bridge suppresses it although it was frozen.
        assert_eq!(
            repository.owner_discard("job-1", None, "job gone", false, at).unwrap().unwrap_err(),
            OwnerDiscardError::ProtectedControl
        );
        let (job, events) =
            capture_events(|| repository.owner_discard("job-1", None, "job gone", true, at));
        let job = job.unwrap().unwrap();
        assert_eq!(job.snapshot.state, "quarantined");
        assert!(events.is_empty(), "no agent sender, no messageFailed");
        assert!(repository.suppress_native_delivery(TO, "job-1").unwrap());
        assert_eq!(repository.stuck_messages(0, NOW).unwrap().len(), 1);

        // Quarantined by the r214 owner script: counts as owner-discarded.
        repository.record_queued_isolated(agent_message("scripted", 10)).unwrap();
        freeze(&repository, "scripted");
        let _ = capture_events(|| {
            repository
                .record_quarantined(
                    TO,
                    "scripted",
                    json!({"source": "human", "code": "stale_message_discarded", "message": "old"}),
                    Utc::now(),
                )
                .unwrap()
        });
        assert!(repository.suppress_native_delivery(TO, "scripted").unwrap());
        assert!(!repository.owner_discard("scripted", None, "again", false, at).unwrap().unwrap().changed);

        // Terminal states are refused.
        let _ = capture_events(|| {
            repository
                .record_failed(TO, "fresh", json!({"code": "x"}), Utc::now())
                .unwrap()
        });
        assert_eq!(
            repository.owner_discard("fresh", None, "late", false, at).unwrap().unwrap_err(),
            OwnerDiscardError::NotDiscardable {
                state: "failed".into()
            }
        );
        let _ = fs::remove_dir_all(root);
    }

    struct FakeBus {
        queued: RefCell<Vec<QueuedAgentBusMessage>>,
        available: bool,
        withdrawn: RefCell<Vec<String>>,
    }

    impl LiveBus for FakeBus {
        fn queued(&self) -> anyhow::Result<Vec<QueuedAgentBusMessage>> {
            anyhow::ensure!(self.available, "bus down");
            Ok(self.queued.borrow().clone())
        }

        fn withdraw(
            &self,
            ids: &[String],
            _reason: &str,
        ) -> anyhow::Result<Vec<(String, String)>> {
            anyhow::ensure!(self.available, "bus down");
            self.withdrawn.borrow_mut().extend(ids.iter().cloned());
            let mut queued = self.queued.borrow_mut();
            let removed = queued
                .iter()
                .filter(|q| ids.contains(&q.message_id))
                .map(|q| (q.agent_id.clone(), q.message_id.clone()))
                .collect::<Vec<_>>();
            queued.retain(|q| !ids.contains(&q.message_id));
            Ok(removed)
        }
    }

    fn live(id: &str, created: u64, control: Option<&str>, kind: &str) -> QueuedAgentBusMessage {
        QueuedAgentBusMessage {
            message_id: id.into(),
            agent_id: "rt-to".into(),
            agent_name: Some("target".into()),
            to_cutex_session_id: Some(TO.into()),
            created_at_epoch_secs: created,
            sender: "someone".into(),
            sender_kind: kind.into(),
            control_type: control.map(str::to_string),
            content_preview: format!("preview {id}"),
        }
    }

    fn request(id: &str, allow: bool) -> DiscardRequest {
        DiscardRequest {
            message_id: id.into(),
            reason: "owner cleanup".into(),
            expected_semantic_sha256: None,
            allow_protected: allow,
        }
    }

    #[test]
    fn management_list_merges_live_queue_and_discard_withdraws_and_audits() {
        let (repository, root) = repo("route");
        repository
            .record_queued_isolated(agent_message("durable-old", 10))
            .unwrap();
        let bus = FakeBus {
            queued: RefCell::new(vec![
                live("durable-old", 10, None, "cutex"),
                live("ghost", 30, None, "cutex"),
                live(
                    "ghost-task",
                    40,
                    Some("cutex.task_service.completion.v1"),
                    "task_service",
                ),
                live("young", NOW - 5, None, "cutex"),
            ]),
            available: true,
            withdrawn: RefCell::new(Vec::new()),
        };
        let names = |id: &str| (id == TO).then(|| "Target Agent".to_string());
        let view =
            agent_bus_stuck::list_view(&repository, &bus, 3600, None, 100, NOW, &names).unwrap();
        let rows = view["messages"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r["messageId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["durable-old", "ghost", "ghost-task"]
        );
        assert_eq!(rows[0]["durable"], true);
        assert_eq!(rows[0]["to"]["name"], "Target Agent");
        assert_eq!(rows[0]["liveQueues"][0]["agentId"], "rt-to");
        assert!(rows[0]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("without ACK")));
        assert_eq!(rows[1]["durable"], false);
        assert_eq!(rows[1]["state"], "live_only");
        assert_eq!(rows[2]["protectedControl"], true);
        assert_eq!(view["liveQueue"]["available"], true);
        let filtered =
            agent_bus_stuck::list_view(&repository, &bus, 3600, Some(FROM), 100, NOW, &names)
                .unwrap();
        assert_eq!(
            filtered["messages"].as_array().unwrap().len(),
            1,
            "only the agent message has FROM"
        );
        let limited =
            agent_bus_stuck::list_view(&repository, &bus, 3600, None, 1, NOW, &names).unwrap();
        assert_eq!(limited["total"], 3);
        assert_eq!(limited["messages"].as_array().unwrap().len(), 1);

        let facts = RefCell::new(Vec::new());
        let audit = |fact: Value| facts.borrow_mut().push(fact);
        let (result, _) = capture_events(|| {
            agent_bus_stuck::discard(&repository, &bus, &request("durable-old", false), Utc::now(), &audit)
        });
        let result = result.unwrap().unwrap();
        assert_eq!(result["changed"], true);
        assert_eq!(result["durable"]["state"], "discarded");
        assert_eq!(result["liveWithdrawn"][0]["agentId"], "rt-to");
        assert_eq!(facts.borrow()[0]["event"], "owner_discarded");
        assert_eq!(facts.borrow()[0]["message_id"], "durable-old");
        // Again: idempotent, still answers 200 and retries the withdrawal.
        let again = agent_bus_stuck::discard(
            &repository,
            &bus,
            &request("durable-old", false),
            Utc::now(),
            &audit,
        )
        .unwrap()
        .unwrap();
        assert_eq!(again["changed"], false);
        assert_eq!(bus.withdrawn.borrow().len(), 2);

        // Live-only: withdrawn; protected needs allowProtected.
        let ghost =
            agent_bus_stuck::discard(&repository, &bus, &request("ghost", false), Utc::now(), &audit)
                .unwrap()
                .unwrap();
        assert_eq!(ghost["durable"], Value::Null);
        assert_eq!(ghost["changed"], true);
        let refused = agent_bus_stuck::discard(
            &repository,
            &bus,
            &request("ghost-task", false),
            Utc::now(),
            &audit,
        )
        .unwrap()
        .unwrap_err();
        assert_eq!((refused.status, refused.code.as_str()), (409, "message_protected"));
        assert!(agent_bus_stuck::discard(
            &repository,
            &bus,
            &request("ghost-task", true),
            Utc::now(),
            &audit
        )
        .unwrap()
        .is_ok());
        let missing = agent_bus_stuck::discard(
            &repository,
            &bus,
            &request("nowhere", true),
            Utc::now(),
            &audit,
        )
        .unwrap()
        .unwrap_err();
        assert_eq!((missing.status, missing.code.as_str()), (404, "message_not_found"));

        // Bus down: the list says so; a durable discard still lands.
        repository
            .record_queued_isolated(agent_message("durable-2", 10))
            .unwrap();
        let down = FakeBus {
            queued: RefCell::new(Vec::new()),
            available: false,
            withdrawn: RefCell::new(Vec::new()),
        };
        let view =
            agent_bus_stuck::list_view(&repository, &down, 3600, None, 100, NOW, &names).unwrap();
        assert_eq!(view["liveQueue"]["available"], false);
        assert_eq!(view["messages"].as_array().unwrap().len(), 1);
        let (result, _) = capture_events(|| {
            agent_bus_stuck::discard(&repository, &down, &request("durable-2", false), Utc::now(), &audit)
        });
        let result = result.unwrap().unwrap();
        assert_eq!(result["durable"]["changed"], true);
        assert!(result["liveWithdrawError"]
            .as_str()
            .unwrap()
            .contains("bus down"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discard_request_parsing_is_strict() {
        let sha = format!("ABCDEF{}", "0".repeat(58));
        let body = json!({
            "messageId": "m",
            "reason": " r ",
            "allowProtected": true,
            "expectedSemanticSha256": sha,
        });
        let ok = DiscardRequest::parse(body.to_string().as_bytes()).unwrap();
        assert_eq!(ok.reason, "r");
        assert!(ok.allow_protected);
        assert_eq!(ok.expected_semantic_sha256.unwrap(), sha.to_ascii_lowercase());
        for bad in [
            json!({"reason": "r"}),
            json!({"messageId": "m"}),
            json!({"messageId": "m", "reason": "r", "extra": 1}),
            json!({"messageId": "m", "reason": "r", "expectedSemanticSha256": "xyz"}),
            json!({"messageId": "m", "reason": "r", "allowProtected": "yes"}),
            json!([]),
        ] {
            assert_eq!(
                DiscardRequest::parse(bad.to_string().as_bytes()).unwrap_err().code,
                "invalid_request",
                "{bad}"
            );
        }
        assert!(agent_bus_stuck::is_path("/v2/agent-bus/stuck-messages?minAgeSecs=1"));
        assert!(agent_bus_stuck::is_path("/v2/agent-bus/stuck-messages/discard"));
        assert!(!agent_bus_stuck::is_path("/v2/agent-bus/stuck-messages/x"));
    }
}
