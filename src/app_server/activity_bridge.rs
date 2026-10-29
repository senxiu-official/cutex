//! Durable projection from owner-scoped Management v2 facts to the app-server
//! `thread/cutexActivity` UI-only lane.

use std::collections::BTreeMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use chrono::DateTime;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::commands::{
    AppServerCommands, CutexUiActivity, CutexUiActivityCheckpoint, CutexUiActivityDelivery,
    CutexUiActivityDeliveryClass, CutexUiActivityDeliverySchema,
    CutexUiActivityIngestionDisposition, ManagedAgentActionPhase, ManagedAgentActivityItem,
    ManagedAgentActivityStatus, ManagedAgentOperation, ManagedAgentReplacePolicy,
    ManagedAgentRotationMode, OutboundInterAgentMessageItem, OutboundInterAgentMessageStatus,
    ThreadCutexActivityParams,
};
use super::manager::AppServerRuntimeManager;
use super::participants::{ParticipantMetadataResolver, RegistryParticipantMetadataResolver};
use crate::agent_bus::delivery::AgentDeliveryMode;
use crate::agent_management::{
    AgentActionPhase, AgentManagementFailureEvent, AgentManagementReceipt, AgentManagementResult,
    AgentOperationKind, AgentReplacePolicy, DirectorRotateMode,
};
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::config::paths::runtime_dir;
use crate::management::v2::integration_events::{
    AgentManagementPhaseTransitionFact, AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
    AGENT_MANAGEMENT_PHASE_TRANSITION_SCHEMA,
};
use crate::management::v2::model::EventEnvelope;
use crate::management::v2::repository::{
    management_v2_repository, EventRepository, ReplayError, ReplayQuery,
};

const PROJECTION_STATE_VERSION: u8 = 1;
const PROJECTOR_PAGE_LIMIT: usize = 1000;
const PROJECTOR_DELIVERY_BATCH_LIMIT: usize = 32;
const PROJECTOR_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// A pass that only moves scan cursors (no new pending entry, no delivery)
/// is persisted at most this often. Re-scanning from an older cursor after a
/// crash is harmless: events at or below the durable pending/ack sequences are
/// skipped, and anything that would be queued forces a full save first.
/// Writing the whole state for every cursor step rewrote 26 MB once per owner
/// per new event on the Linux host (2026-10-09).
const PROJECTOR_CURSOR_SAVE_INTERVAL: Duration = Duration::from_secs(30);
/// Undelivered entries kept per owner. Owners whose runtime cannot ingest
/// activity (stock runtimes) never drain; their queues grew to thousands of
/// entries and made every save write tens of MB. The oldest entries not yet
/// handed to transport are dropped first.
const PROJECTOR_MAX_PENDING_PER_OWNER: usize = 256;
const PREVIEW_LIMIT: usize = 2_000;
const DETAIL_LIMIT: usize = 2_000;

const ACTION_COMPLETED: &str = "cutex/agentManagement/actionCompleted";
const ACTION_FAILED: &str = "cutex/agentManagement/actionFailed";
const MESSAGE_SENT: &str = "cutex/agentBus/messageSent";
/// `type` tags of the activity items r219 removed with the Task Service. A
/// pending projector entry an older release queued with one of them loads as
/// an intentional skip (`activity: None`), see [`PendingProjection`].
const REMOVED_ACTIVITY_TYPES: [&str; 2] = ["taskAssignmentActivity", "taskWatchdogActivity"];

pub trait ActivitySubmitter: Send + Sync + 'static {
    fn submit(
        &self,
        owner_cutex_session_id: &str,
        delivery: CutexUiActivityDelivery,
        activity: CutexUiActivity,
    ) -> anyhow::Result<CutexUiActivityIngestionDisposition>;
}

impl ActivitySubmitter for AppServerRuntimeManager {
    fn submit(
        &self,
        owner_cutex_session_id: &str,
        delivery: CutexUiActivityDelivery,
        activity: CutexUiActivity,
    ) -> anyhow::Result<CutexUiActivityIngestionDisposition> {
        anyhow::ensure!(
            self.supports_cutex_ingress(owner_cutex_session_id)?,
            "stock runtime does not support Cutex activity ingress"
        );
        let status = self
            .status(owner_cutex_session_id)?
            .with_context(|| format!("owner runtime is offline: {owner_cutex_session_id}"))?;
        if !status.connected {
            anyhow::bail!("owner runtime is offline: {owner_cutex_session_id}");
        }
        let handle = self
            .handle_for_generation(owner_cutex_session_id, status.runtime_generation)
            .map_err(anyhow::Error::new)?;
        AppServerCommands::new(handle)
            .thread_cutex_activity(&ThreadCutexActivityParams {
                thread_id: status.thread_id,
                delivery,
                activity,
            })
            .map(|response| response.disposition)
            .map_err(anyhow::Error::new)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MappingDisposition {
    Project(Box<CutexUiActivity>),
    Skip,
    Ignore,
}

pub struct ActivityMapper<P> {
    participants: P,
}

impl<P> ActivityMapper<P>
where
    P: ParticipantMetadataResolver,
{
    pub fn new(participants: P) -> Self {
        Self { participants }
    }

    fn map(&self, event: &EventEnvelope) -> anyhow::Result<MappingDisposition> {
        let Some(cutex) = &event.cutex else {
            return Ok(MappingDisposition::Ignore);
        };
        match cutex.method.as_str() {
            ACTION_COMPLETED => self.map_agent_completed(event),
            ACTION_FAILED => self.map_agent_failed(event),
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD => self.map_agent_phase(event),
            MESSAGE_SENT => self.map_message_sent(event),
            // r219: Task Service / watchdog facts left in the stream by an
            // older release are ignored like any other unsupported method.
            _ => Ok(MappingDisposition::Ignore),
        }
    }

    /// [`Self::map`] for the durable scans: an event whose payload can never
    /// be decoded (a shape this build does not know) is skipped with one
    /// warning instead of failing every pass at the same cursor forever.
    /// Other errors (an unavailable provider) stay retryable.
    fn map_or_skip(&self, event: &EventEnvelope) -> anyhow::Result<MappingDisposition> {
        match self.map(event) {
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                let method = event.cutex.as_ref().map_or("", |cutex| cutex.method.as_str());
                eprintln!(
                    "\x1b[33mwarning:\x1b[0m Cutex activity projector skipped undecodable {method} event {} (sequence {}): {error:#}",
                    event.event_id, event.sequence
                );
                Ok(MappingDisposition::Skip)
            }
            result => result,
        }
    }

    fn map_agent_completed(&self, event: &EventEnvelope) -> anyhow::Result<MappingDisposition> {
        let receipt: AgentManagementReceipt =
            serde_json::from_value(event.cutex.as_ref().expect("checked").params.clone())?;
        if matches!(
            receipt.operation,
            AgentOperationKind::QueryManaged
                | AgentOperationKind::GrantOperator
                | AgentOperationKind::RevokeOperator
                | AgentOperationKind::DirectorRotate
        ) {
            // A Director rotation publishes exact, audience-switched phase
            // facts through AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD. Projecting
            // its legacy caller-owned receipt too would create a second,
            // phase-less completion that bypasses the director_rotate.complete
            // presentation rule.
            return Ok(MappingDisposition::Skip);
        }
        let (agent, observation) = match &receipt.result {
            AgentManagementResult::Created {
                agent, observation, ..
            }
            | AgentManagementResult::Lifecycle { agent, observation } => (agent, observation),
            AgentManagementResult::Replaced {
                successor,
                observation,
                ..
            }
            | AgentManagementResult::DirectorRotated {
                successor,
                observation,
                ..
            } => (successor, observation),
            AgentManagementResult::QueryManaged { .. }
            | AgentManagementResult::OperatorGranted { .. }
            | AgentManagementResult::OperatorRevoked { .. } => {
                return Ok(MappingDisposition::Skip);
            }
        };
        let Some(operation) = managed_operation(receipt.operation) else {
            return Ok(MappingDisposition::Skip);
        };
        let metadata = self.participants.resolve(agent.cutex_session_id.as_str());
        Ok(MappingDisposition::Project(Box::new(
            CutexUiActivity::ManagedAgentActivity(ManagedAgentActivityItem {
                id: exact_bounded(receipt.action_id.as_str(), 512, "activity id")?,
                event_id: exact_bounded(&event.event_id, 512, "eventId")?,
                sequence: event.sequence,
                occurred_at_ms: timestamp_ms(receipt.completed_at.as_str())?,
                // actionCompleted is the legacy one-shot lifecycle projection.  It is not an
                // authoritative phase-transition fact and therefore must not populate the
                // phase-only fields.  Real phase facts arrive through
                // AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD with a phase_event_id.
                action_id: None,
                phase_event_id: None,
                phase: None,
                operation,
                status: ManagedAgentActivityStatus::Completed,
                managed_agent_id: exact_bounded(
                    agent.cutex_session_id.as_str(),
                    512,
                    "managedAgentId",
                )?,
                managed_agent_name: metadata.display_name.clone(),
                managed_agent_metadata: Some(metadata),
                // A launch profile is not an authoritative role assertion.
                managed_agent_role: None,
                initial_task_preview: None,
                detail: None,
                runtime_generation: Some(observation.runtime_generation),
                predecessor_agent_id: None,
                predecessor_agent_name: None,
                predecessor_metadata: None,
                successor_agent_id: None,
                successor_agent_name: None,
                successor_metadata: None,
                replace_policy: None,
                rotation_mode: None,
                authority_epoch: None,
            }),
        )))
    }

    fn map_agent_failed(&self, event: &EventEnvelope) -> anyhow::Result<MappingDisposition> {
        let failure: AgentManagementFailureEvent =
            serde_json::from_value(event.cutex.as_ref().expect("checked").params.clone())?;
        let Some(target) = failure.target_cutex_session_id.as_ref() else {
            return Ok(MappingDisposition::Skip);
        };
        let Some(operation) = managed_operation(failure.operation) else {
            return Ok(MappingDisposition::Skip);
        };
        let metadata = self.participants.resolve(target.as_str());
        Ok(MappingDisposition::Project(Box::new(
            CutexUiActivity::ManagedAgentActivity(ManagedAgentActivityItem {
                id: exact_bounded(failure.action_id.as_str(), 512, "activity id")?,
                event_id: exact_bounded(&event.event_id, 512, "eventId")?,
                sequence: event.sequence,
                occurred_at_ms: timestamp_ms(failure.created_at.as_str())?,
                // actionFailed is likewise a legacy terminal projection, not a typed phase
                // transition.  Leaving any phase-only field populated makes the downstream
                // app-server reject the entire owner queue.
                action_id: None,
                phase_event_id: None,
                phase: None,
                operation,
                status: ManagedAgentActivityStatus::Failed,
                managed_agent_id: exact_bounded(target.as_str(), 512, "managedAgentId")?,
                managed_agent_name: metadata.display_name.clone(),
                managed_agent_metadata: Some(metadata),
                managed_agent_role: None,
                initial_task_preview: None,
                detail: Some(bounded(&failure.detail, DETAIL_LIMIT)),
                runtime_generation: None,
                predecessor_agent_id: None,
                predecessor_agent_name: None,
                predecessor_metadata: None,
                successor_agent_id: None,
                successor_agent_name: None,
                successor_metadata: None,
                replace_policy: None,
                rotation_mode: None,
                authority_epoch: None,
            }),
        )))
    }

    fn map_agent_phase(&self, event: &EventEnvelope) -> anyhow::Result<MappingDisposition> {
        let fact: AgentManagementPhaseTransitionFact =
            serde_json::from_value(event.cutex.as_ref().expect("checked").params.clone())?;
        if fact.schema != AGENT_MANAGEMENT_PHASE_TRANSITION_SCHEMA {
            anyhow::bail!("Agent Management phase fact has an unsupported schema");
        }
        if fact.primary_presentation_target_cutex_session_id != event.cutex_session_id {
            anyhow::bail!("Agent Management phase presentation target disagrees with event owner");
        }
        let Some(operation) = managed_operation(fact.operation) else {
            return Ok(MappingDisposition::Skip);
        };
        let phase = managed_phase(fact.phase);
        let status = match fact.phase {
            AgentActionPhase::Complete => ManagedAgentActivityStatus::Completed,
            AgentActionPhase::NoWrite
            | AgentActionPhase::OwnerActionRequired
            | AgentActionPhase::Failure => ManagedAgentActivityStatus::Failed,
            _ => ManagedAgentActivityStatus::InProgress,
        };
        let legacy_predecessor_subject = fact.operation == AgentOperationKind::DirectorRotate
            && matches!(
                fact.phase,
                AgentActionPhase::PredecessorClosing | AgentActionPhase::PredecessorClosed
            );
        let subject_cutex_session_id = fact.subject_cutex_session_id.clone().or_else(|| {
            // Legacy phase facts did not carry an explicit subject. Preserve
            // phase semantics without falling back to the presentation owner.
            if legacy_predecessor_subject {
                fact.predecessor_cutex_session_id.clone()
            } else {
                fact.successor_cutex_session_id.clone()
            }
        });
        let subject_agent_id = subject_cutex_session_id.clone().unwrap_or_else(|| {
            if fact.subject_agent_name.is_some() {
                format!("pending:{}", fact.action_id)
            } else {
                // Compatibility fallback for retained v1 facts only.
                fact.primary_presentation_target_cutex_session_id.clone()
            }
        });
        let subject_metadata = subject_cutex_session_id.as_ref().map(|session_id| {
            if fact.successor_cutex_session_id.as_ref() == Some(session_id) {
                fact.successor_metadata
                    .clone()
                    .unwrap_or_else(|| self.participants.resolve(session_id))
            } else if fact.predecessor_cutex_session_id.as_ref() == Some(session_id) {
                fact.predecessor_metadata
                    .clone()
                    .unwrap_or_else(|| self.participants.resolve(session_id))
            } else {
                self.participants.resolve(session_id)
            }
        });
        let subject_agent_name = fact.subject_agent_name.clone().or_else(|| {
            subject_metadata
                .as_ref()
                .and_then(|metadata| metadata.display_name.clone())
        });
        let predecessor_name = fact
            .predecessor_metadata
            .as_ref()
            .and_then(|metadata| metadata.display_name.clone());
        let successor_name = fact
            .successor_metadata
            .as_ref()
            .and_then(|metadata| metadata.display_name.clone())
            .or_else(|| {
                matches!(
                    fact.operation,
                    AgentOperationKind::Replace | AgentOperationKind::DirectorRotate
                )
                .then(|| fact.subject_agent_name.clone())
                .flatten()
            });
        Ok(MappingDisposition::Project(Box::new(
            CutexUiActivity::ManagedAgentActivity(ManagedAgentActivityItem {
                id: exact_bounded(&fact.action_id, 512, "activity id")?,
                event_id: exact_bounded(&event.event_id, 512, "eventId")?,
                sequence: event.sequence,
                occurred_at_ms: timestamp_ms(&fact.committed_at)?,
                action_id: Some(exact_bounded(&fact.action_id, 512, "actionId")?),
                phase_event_id: Some(exact_bounded(&fact.phase_event_id, 512, "phaseEventId")?),
                phase: Some(phase),
                operation,
                status,
                managed_agent_id: exact_bounded(&subject_agent_id, 512, "managedAgentId")?,
                managed_agent_name: subject_agent_name,
                managed_agent_metadata: subject_metadata,
                managed_agent_role: None,
                initial_task_preview: None,
                detail: None,
                runtime_generation: None,
                predecessor_agent_id: fact.predecessor_cutex_session_id,
                predecessor_agent_name: predecessor_name,
                predecessor_metadata: fact.predecessor_metadata,
                successor_agent_id: fact.successor_cutex_session_id,
                successor_agent_name: successor_name,
                successor_metadata: fact.successor_metadata,
                replace_policy: fact.replace_policy.map(managed_replace_policy),
                rotation_mode: fact.rotation_mode.map(managed_rotation_mode),
                authority_epoch: fact.authority_epoch,
            }),
        )))
    }

    fn map_message_sent(&self, event: &EventEnvelope) -> anyhow::Result<MappingDisposition> {
        // Agent messaging v2 adds `body, deliveryKind, sender, recipient` and
        // sender-fact flags (`agent_bus_state::agent_fields`); unknown fields
        // are accepted so a newer producer never wedges the projector.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MessageSent {
            message_id: String,
            from_cutex_session_id: String,
            to_cutex_session_id: String,
            delivery_mode: AgentDeliveryMode,
            #[serde(default)]
            content: Option<String>,
            #[serde(default)]
            body: Option<String>,
            sent_at: String,
        }
        let sent: MessageSent =
            serde_json::from_value(event.cutex.as_ref().expect("checked").params.clone())?;
        let content = sent
            .content
            .or(sent.body)
            .context("Agent Bus message event has neither content nor body")?;
        if sent.from_cutex_session_id != event.cutex_session_id {
            anyhow::bail!("Agent Bus sender does not match the owner-scoped event");
        }
        let sender_metadata = self.participants.resolve(&sent.from_cutex_session_id);
        let recipient_metadata = self.participants.resolve(&sent.to_cutex_session_id);
        Ok(MappingDisposition::Project(Box::new(
            CutexUiActivity::OutboundInterAgentMessage(OutboundInterAgentMessageItem {
                id: exact_bounded(&sent.message_id, 512, "activity id")?,
                event_id: exact_bounded(&event.event_id, 512, "eventId")?,
                sequence: event.sequence,
                occurred_at_ms: timestamp_ms(&sent.sent_at)?,
                sender_agent_id: exact_bounded(&sent.from_cutex_session_id, 512, "senderAgentId")?,
                sender_agent_name: sender_metadata.display_name.clone(),
                sender_metadata: Some(sender_metadata),
                recipient_agent_id: exact_bounded(
                    &sent.to_cutex_session_id,
                    512,
                    "recipientAgentId",
                )?,
                recipient_agent_name: recipient_metadata.display_name.clone(),
                recipient_metadata: Some(recipient_metadata),
                other_recipient_agent_ids: Vec::new(),
                delivery_mode: sent.delivery_mode,
                status: OutboundInterAgentMessageStatus::Sent,
                content_preview: Some(bounded(&content, PREVIEW_LIMIT)),
                detail: None,
            }),
        )))
    }
}

fn managed_operation(operation: AgentOperationKind) -> Option<ManagedAgentOperation> {
    match operation {
        AgentOperationKind::Create => Some(ManagedAgentOperation::Create),
        AgentOperationKind::Online => Some(ManagedAgentOperation::Online),
        AgentOperationKind::Offline => Some(ManagedAgentOperation::Offline),
        AgentOperationKind::Restart => Some(ManagedAgentOperation::Restart),
        AgentOperationKind::Replace => Some(ManagedAgentOperation::Replace),
        AgentOperationKind::Close => Some(ManagedAgentOperation::Close),
        AgentOperationKind::DirectorRotate => Some(ManagedAgentOperation::DirectorRotate),
        AgentOperationKind::QueryManaged
        | AgentOperationKind::GrantOperator
        | AgentOperationKind::RevokeOperator => None,
    }
}

fn managed_phase(phase: AgentActionPhase) -> ManagedAgentActionPhase {
    match phase {
        AgentActionPhase::Prepared => ManagedAgentActionPhase::Prepared,
        AgentActionPhase::PrivateCwdReady => ManagedAgentActionPhase::PrivateCwdReady,
        AgentActionPhase::NativeBootstrapPending => ManagedAgentActionPhase::NativeBootstrapPending,
        AgentActionPhase::NativeSessionCaptured => ManagedAgentActionPhase::NativeSessionCaptured,
        AgentActionPhase::Adopted => ManagedAgentActionPhase::Adopted,
        AgentActionPhase::Configured => ManagedAgentActionPhase::Configured,
        AgentActionPhase::Online => ManagedAgentActionPhase::Online,
        AgentActionPhase::Ready => ManagedAgentActionPhase::Ready,
        AgentActionPhase::MessagePending => ManagedAgentActionPhase::MessagePending,
        AgentActionPhase::MessageQueued => ManagedAgentActionPhase::MessageQueued,
        AgentActionPhase::PredecessorClosing => ManagedAgentActionPhase::PredecessorClosing,
        AgentActionPhase::PredecessorClosed => ManagedAgentActionPhase::PredecessorClosed,
        AgentActionPhase::AuthorityTransferPending => {
            ManagedAgentActionPhase::AuthorityTransferPending
        }
        AgentActionPhase::AuthorityTransferred => ManagedAgentActionPhase::AuthorityTransferred,
        AgentActionPhase::SuccessorReady => ManagedAgentActionPhase::SuccessorReady,
        AgentActionPhase::Complete => ManagedAgentActionPhase::Complete,
        AgentActionPhase::NoWrite => ManagedAgentActionPhase::NoWrite,
        AgentActionPhase::OwnerActionRequired => ManagedAgentActionPhase::OwnerActionRequired,
        AgentActionPhase::Failure => ManagedAgentActionPhase::Failure,
    }
}

fn managed_replace_policy(policy: AgentReplacePolicy) -> ManagedAgentReplacePolicy {
    match policy {
        AgentReplacePolicy::CloseBeforeCreate => ManagedAgentReplacePolicy::CloseBeforeCreate,
        AgentReplacePolicy::CloseAfterReady => ManagedAgentReplacePolicy::CloseAfterReady,
        AgentReplacePolicy::KeepOld => ManagedAgentReplacePolicy::KeepOld,
    }
}

fn managed_rotation_mode(mode: DirectorRotateMode) -> ManagedAgentRotationMode {
    match mode {
        DirectorRotateMode::ClosePredecessorThenCreateWithMessage => {
            ManagedAgentRotationMode::ClosePredecessorThenCreateWithMessage
        }
        DirectorRotateMode::RetainPredecessorWithMessage => {
            ManagedAgentRotationMode::RetainPredecessorWithMessage
        }
        DirectorRotateMode::RetainPredecessorBootstrapOnly => {
            ManagedAgentRotationMode::RetainPredecessorBootstrapOnly
        }
    }
}

fn timestamp_ms(value: &str) -> anyhow::Result<i64> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid activity occurrence time: {value}"))?
        .timestamp_millis())
}

fn bounded(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        value.chars().take(max_chars).collect()
    }
}

fn exact_bounded(value: &str, max_chars: usize, field: &str) -> anyhow::Result<String> {
    if value.trim().is_empty() {
        anyhow::bail!("{field} must not be empty");
    }
    if value.chars().count() > max_chars {
        anyhow::bail!("{field} exceeds the frozen UI contract");
    }
    Ok(value.to_string())
}

fn supported_method(event: &EventEnvelope) -> bool {
    event.cutex.as_ref().is_some_and(|cutex| {
        matches!(
            cutex.method.as_str(),
            ACTION_COMPLETED
                | ACTION_FAILED
                | AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD
                | MESSAGE_SENT
        )
    })
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct OwnerProjectionCursor {
    stream_id: Option<String>,
    cursor: Option<String>,
    sequence: u64,
    #[serde(default)]
    scan_cursor: Option<String>,
    #[serde(default)]
    pending: Vec<PendingProjection>,
    #[serde(default)]
    live_acknowledged_sequence: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PendingProjection {
    stream_id: String,
    cursor: String,
    sequence: u64,
    #[serde(default = "default_delivery_class")]
    delivery_class: CutexUiActivityDeliveryClass,
    #[serde(default)]
    delivery: Option<CutexUiActivityDelivery>,
    #[serde(default = "default_recovered_delivery")]
    recovered: bool,
    /// `None` records an intentional skip/ignore so acknowledgement remains
    /// ordered behind any earlier activity for the same owner. An entry an
    /// older release queued with a removed Task Service item type loads as
    /// `None` too ([`deserialize_pending_activity`]).
    #[serde(default, deserialize_with = "deserialize_pending_activity")]
    activity: Option<CutexUiActivity>,
}

/// r219: tolerant decode of [`PendingProjection::activity`]. The state file
/// may still hold `taskAssignmentActivity` / `taskWatchdogActivity` entries
/// queued before the Task Service was removed; they become skips instead of
/// making the whole projector state unreadable. Any other shape is still
/// strict.
fn deserialize_pending_activity<'de, D>(deserializer: D) -> Result<Option<CutexUiActivity>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(value) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| REMOVED_ACTIVITY_TYPES.contains(&kind))
    {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some).map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ProjectionState {
    version: u8,
    stream_id: Option<String>,
    discovery_cursor: Option<String>,
    /// Global scan boundary captured at activation and then advanced only
    /// after the corresponding live records are durably queued.
    #[serde(default)]
    live_cursor: Option<String>,
    #[serde(default)]
    live_sequence: u64,
    #[serde(default)]
    live_origin_sequence: u64,
    #[serde(default)]
    live_initialized: bool,
    owners: BTreeMap<String, OwnerProjectionCursor>,
}

impl Default for ProjectionState {
    fn default() -> Self {
        Self {
            version: PROJECTION_STATE_VERSION,
            stream_id: None,
            discovery_cursor: None,
            live_cursor: None,
            live_sequence: 0,
            live_origin_sequence: 0,
            live_initialized: false,
            owners: BTreeMap::new(),
        }
    }
}

struct ProjectionStateStore {
    path: PathBuf,
    state: ProjectionState,
    /// Successful saves so far: a pass that saved nothing changed nothing.
    saves: std::cell::Cell<u64>,
    /// When the state was last written (`None`: not by this process).
    last_save: std::cell::Cell<Option<std::time::Instant>>,
    /// State changes so far, written or not (a cursor-only change may be
    /// deferred): a pass that changed nothing is idle.
    changes: std::cell::Cell<u64>,
}

/// Drop the oldest entries of an owner queue beyond
/// [`PROJECTOR_MAX_PENDING_PER_OWNER`], never one already handed to transport.
fn bound_pending(pending: &mut Vec<PendingProjection>) {
    let mut excess = pending
        .len()
        .saturating_sub(PROJECTOR_MAX_PENDING_PER_OWNER);
    if excess == 0 {
        return;
    }
    pending.retain(|entry| {
        if excess > 0 && entry.delivery.is_none() {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

impl ProjectionStateStore {
    fn open(path: PathBuf) -> anyhow::Result<Self> {
        let (mut state, recovered) = match fs::read(&path) {
            Ok(bytes) => (
                serde_json::from_slice(&bytes).with_context(|| {
                    format!("invalid activity projector state: {}", path.display())
                })?,
                true,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (ProjectionState::default(), false)
            }
            Err(error) => return Err(error).context("failed to read activity projector state"),
        };
        if state.version != PROJECTION_STATE_VERSION {
            anyhow::bail!("unsupported activity projector state version");
        }
        if recovered {
            for owner in state.owners.values_mut() {
                for pending in &mut owner.pending {
                    pending.recovered = true;
                    if let Some(delivery) = pending.delivery.as_mut() {
                        delivery.recovered = true;
                    }
                }
            }
            write_private_pretty_json_atomic(&path, &state, "activity projector state")?;
        }
        Ok(Self {
            path,
            state,
            saves: std::cell::Cell::new(0),
            last_save: std::cell::Cell::new(None),
            changes: std::cell::Cell::new(0),
        })
    }

    fn save(&self) -> anyhow::Result<()> {
        write_private_pretty_json_atomic(&self.path, &self.state, "activity projector state")?;
        self.saves.set(self.saves.get() + 1);
        self.changes.set(self.changes.get() + 1);
        self.last_save.set(Some(std::time::Instant::now()));
        Ok(())
    }

    /// Save a cursor-only change unless the state was written within
    /// [`PROJECTOR_CURSOR_SAVE_INTERVAL`]; the in-memory cursor still moves and
    /// is written with the next save.
    fn save_cursor(&self) -> anyhow::Result<()> {
        self.changes.set(self.changes.get() + 1);
        if self
            .last_save
            .get()
            .is_some_and(|at| at.elapsed() < PROJECTOR_CURSOR_SAVE_INTERVAL)
        {
            return Ok(());
        }
        self.save()
    }

    fn reset_stream(&mut self, stream_id: String) -> anyhow::Result<()> {
        let before = self.state.clone();
        self.state.stream_id = Some(stream_id.clone());
        self.state.discovery_cursor = None;
        self.state.live_cursor = None;
        self.state.live_sequence = 0;
        self.state.live_origin_sequence = 0;
        self.state.live_initialized = false;
        for owner in self.state.owners.values_mut() {
            owner.stream_id = Some(stream_id.clone());
            owner.cursor = None;
            owner.sequence = 0;
            owner.scan_cursor = None;
            owner.live_acknowledged_sequence = 0;
            // Pending entries contain enough authoritative presentation data
            // to survive source retention or stream recovery and are replayed
            // before the new stream is scanned.
            for pending in &mut owner.pending {
                pending.delivery_class = CutexUiActivityDeliveryClass::CatchUp;
                pending.delivery = None;
                pending.recovered = true;
            }
        }
        if let Err(error) = self.save() {
            self.state = before;
            return Err(error);
        }
        Ok(())
    }
}

fn default_delivery_class() -> CutexUiActivityDeliveryClass {
    CutexUiActivityDeliveryClass::CatchUp
}

fn default_recovered_delivery() -> bool {
    true
}

struct ActivityProjector<S, P> {
    state: ProjectionStateStore,
    submitter: S,
    mapper: ActivityMapper<P>,
    reported_owner_errors: BTreeMap<String, String>,
    /// Owners whose last delivery failed (offline, stock runtime, stream
    /// moved on): retried after an exponential delay instead of every pass.
    owner_backoff: BTreeMap<String, OwnerBackoff>,
    /// `(stream, latest sequence)` of the last pass that changed nothing.
    /// While the stream has no new event and no backoff expired, the next
    /// pass would do exactly the same work, so it is skipped.
    idle_at: Option<(String, Option<u64>)>,
}

#[derive(Debug, Clone, Copy)]
struct OwnerBackoff {
    until: std::time::Instant,
    delay: Duration,
}

const OWNER_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const OWNER_BACKOFF_MAX: Duration = Duration::from_secs(30);

impl<S, P> ActivityProjector<S, P>
where
    S: ActivitySubmitter,
    P: ParticipantMetadataResolver,
{
    fn new(state_path: PathBuf, submitter: S, participants: P) -> anyhow::Result<Self> {
        Ok(Self {
            state: ProjectionStateStore::open(state_path)?,
            submitter,
            mapper: ActivityMapper::new(participants),
            reported_owner_errors: BTreeMap::new(),
            owner_backoff: BTreeMap::new(),
            idle_at: None,
        })
    }

    fn backed_off(&self, owner: &str, now: std::time::Instant) -> bool {
        self.owner_backoff
            .get(owner)
            .is_some_and(|backoff| backoff.until > now)
    }

    fn note_owner_failure(&mut self, owner: &str, now: std::time::Instant) {
        // The first failure retries on the next pass (a changed endpoint
        // generation heals at once); a persistent one waits 1 s, 2 s .. 30 s.
        let delay = self
            .owner_backoff
            .get(owner)
            .map_or(Duration::ZERO, |backoff| {
                if backoff.delay.is_zero() {
                    OWNER_BACKOFF_INITIAL
                } else {
                    (backoff.delay * 2).min(OWNER_BACKOFF_MAX)
                }
            });
        self.owner_backoff.insert(
            owner.to_string(),
            OwnerBackoff {
                until: now + delay,
                delay,
            },
        );
    }

    fn run_once(&mut self, repository: &EventRepository) -> anyhow::Result<()> {
        let metadata = repository.stream_metadata()?;
        let now = std::time::Instant::now();
        let position = (
            metadata.stream_id.clone(),
            metadata.latest.as_ref().map(|latest| latest.sequence),
        );
        if self.idle_at.as_ref() == Some(&position)
            && self
                .owner_backoff
                .values()
                .all(|backoff| backoff.until > now)
        {
            return Ok(());
        }
        self.idle_at = None;
        let changes_before = self.state.changes.get();
        if self.state.state.stream_id.as_deref() != Some(&metadata.stream_id) {
            self.state.reset_stream(metadata.stream_id.clone())?;
        }
        self.initialize_live_boundary(&metadata)?;
        self.discover_owners(repository, &metadata.stream_id)?;
        self.fill_live_queues(repository, &metadata.stream_id)?;
        let owners = self
            .state
            .state
            .owners
            .keys()
            .filter(|owner| !self.backed_off(owner, now))
            .cloned()
            .collect::<Vec<_>>();
        let mut errors = BTreeMap::new();
        // Give every owner one bounded live batch before any owner receives
        // historical catch-up bandwidth.
        for owner in &owners {
            if let Err(error) = self.drain_owner_queue(
                &metadata.stream_id,
                owner,
                CutexUiActivityDeliveryClass::Live,
            ) {
                errors.insert(owner.clone(), format!("{error:#}"));
            }
        }
        for owner in &owners {
            if errors.contains_key(owner) {
                continue;
            }
            if let Err(error) = self.project_owner(repository, &metadata.stream_id, owner) {
                errors.insert(owner.clone(), format!("{error:#}"));
            }
        }
        for owner in owners {
            if let Some(detail) = errors.remove(&owner) {
                self.note_owner_failure(&owner, now);
                if self.reported_owner_errors.get(&owner) != Some(&detail) {
                    eprintln!(
                        "\x1b[33mwarning:\x1b[0m Cutex activity projection pending for {owner}: {detail}"
                    );
                    self.reported_owner_errors.insert(owner, detail);
                }
            } else {
                self.owner_backoff.remove(&owner);
                self.reported_owner_errors.remove(&owner);
            }
        }
        // Owners that left the state need no retry clock.
        let known = &self.state.state.owners;
        self.owner_backoff.retain(|owner, _| known.contains_key(owner));
        if self.state.changes.get() == changes_before {
            self.idle_at = Some(position);
        }
        Ok(())
    }

    fn initialize_live_boundary(
        &mut self,
        metadata: &crate::management::v2::model::EventStreamMetadata,
    ) -> anyhow::Result<()> {
        if self.state.state.live_initialized {
            return Ok(());
        }
        let before = self.state.state.clone();
        if let Some(latest) = metadata.latest.as_ref() {
            self.state.state.live_cursor = Some(latest.cursor.clone());
            self.state.state.live_sequence = latest.sequence;
            self.state.state.live_origin_sequence = latest.sequence;
        }
        self.state.state.live_initialized = true;
        // This durable activation boundary classifies every retained record at
        // or before it as catch-up. Anything appended after it enters the live
        // scan regardless of the historical acknowledgement cursor.
        if let Err(error) = self.state.save() {
            self.state.state = before;
            return Err(error);
        }
        Ok(())
    }

    fn fill_live_queues(
        &mut self,
        repository: &EventRepository,
        stream_id: &str,
    ) -> anyhow::Result<()> {
        let page = match repository.page(ReplayQuery {
            stream_id: Some(stream_id.to_string()),
            after: self.state.state.live_cursor.clone(),
            limit: PROJECTOR_PAGE_LIMIT,
            cutex_session_id: None,
        }) {
            Ok(page) => page,
            Err(ReplayError::CursorExpired { .. }) => {
                let before = self.state.state.clone();
                // Pending entries already contain their complete payload. Restart the
                // retained scan without discarding them; `live_sequence` below fences
                // every record that was durably scanned before the cursor expired.
                self.state.state.live_cursor = None;
                if let Err(error) = self.state.save() {
                    self.state.state = before;
                    return Err(error);
                }
                return Ok(());
            }
            Err(error) => return Err(replay_error(error)),
        };
        let previously_scanned_sequence = self.state.state.live_sequence;
        let scanned_through = page.next_cursor;
        let scanned_sequence = page
            .events
            .last()
            .map_or(previously_scanned_sequence, |event| event.sequence)
            .max(previously_scanned_sequence);
        if page.events.is_empty() && scanned_through.is_none() {
            return Ok(());
        }

        let mut additions = Vec::new();
        for event in page.events {
            if event.sequence <= previously_scanned_sequence {
                continue;
            }
            let disposition = self.mapper.map_or_skip(&event)?;
            let activity = match disposition {
                MappingDisposition::Project(activity) => Some(*activity),
                MappingDisposition::Skip | MappingDisposition::Ignore => None,
            };
            if !supported_method(&event) {
                continue;
            }
            additions.push((
                event.cutex_session_id.clone(),
                PendingProjection {
                    stream_id: stream_id.to_string(),
                    cursor: event.cursor,
                    sequence: event.sequence,
                    delivery_class: CutexUiActivityDeliveryClass::Live,
                    delivery: None,
                    recovered: false,
                    activity,
                },
            ));
        }
        // An empty page past the current boundary changes nothing; do not
        // rewrite the (large) state file for it on every pass.
        if additions.is_empty()
            && scanned_through.as_ref().is_none_or(|cursor| {
                self.state.state.live_cursor.as_ref() == Some(cursor)
            })
            && scanned_sequence == previously_scanned_sequence
        {
            return Ok(());
        }

        if additions.is_empty() {
            // Only unsupported events were scanned: move the boundary.
            if let Some(scanned_through) = scanned_through {
                self.state.state.live_cursor = Some(scanned_through);
                self.state.state.live_sequence = scanned_sequence;
            }
            return self.state.save_cursor();
        }
        let before = self.state.state.clone();
        for (owner, pending) in additions {
            let owner_cursor =
                self.state
                    .state
                    .owners
                    .entry(owner)
                    .or_insert_with(|| OwnerProjectionCursor {
                        stream_id: Some(stream_id.to_string()),
                        ..Default::default()
                    });
            owner_cursor.pending.push(pending);
            bound_pending(&mut owner_cursor.pending);
        }
        if let Some(scanned_through) = scanned_through {
            self.state.state.live_cursor = Some(scanned_through);
            self.state.state.live_sequence = scanned_sequence;
        }
        if let Err(error) = self.state.save() {
            self.state.state = before;
            return Err(error);
        }
        Ok(())
    }

    fn discover_owners(
        &mut self,
        repository: &EventRepository,
        stream_id: &str,
    ) -> anyhow::Result<()> {
        let page = match repository.page(ReplayQuery {
            stream_id: Some(stream_id.to_string()),
            after: self.state.state.discovery_cursor.clone(),
            limit: PROJECTOR_PAGE_LIMIT,
            cutex_session_id: None,
        }) {
            Ok(page) => page,
            Err(ReplayError::CursorExpired { .. }) => {
                let before = self.state.state.clone();
                // Owner discovery is idempotent because the owner map is keyed by
                // durable session id. A retained-boundary rewind cannot duplicate it.
                self.state.state.discovery_cursor = None;
                if let Err(error) = self.state.save() {
                    self.state.state = before;
                    return Err(error);
                }
                return Ok(());
            }
            Err(error) => return Err(replay_error(error)),
        };
        let mut changed = false;
        let mut discovered = false;
        for event in &page.events {
            if supported_method(event)
                && !self.state.state.owners.contains_key(&event.cutex_session_id)
            {
                self.state.state.owners.insert(
                    event.cutex_session_id.clone(),
                    OwnerProjectionCursor {
                        stream_id: Some(stream_id.to_string()),
                        ..Default::default()
                    },
                );
                discovered = true;
            }
            self.state.state.discovery_cursor = Some(event.cursor.clone());
            changed = true;
        }
        if discovered {
            self.state.save()?;
        } else if changed {
            // Discovery is idempotent (owners are keyed by session id).
            self.state.save_cursor()?;
        }
        Ok(())
    }

    fn project_owner(
        &mut self,
        repository: &EventRepository,
        stream_id: &str,
        owner: &str,
    ) -> anyhow::Result<()> {
        let fill_error = self.fill_owner_queue(repository, stream_id, owner).err();
        self.drain_owner_queue(stream_id, owner, CutexUiActivityDeliveryClass::CatchUp)?;
        if let Some(error) = fill_error {
            return Err(error);
        }
        Ok(())
    }

    fn fill_owner_queue(
        &mut self,
        repository: &EventRepository,
        stream_id: &str,
        owner: &str,
    ) -> anyhow::Result<()> {
        let after = self
            .state
            .state
            .owners
            .get(owner)
            .and_then(|cursor| cursor.scan_cursor.clone().or_else(|| cursor.cursor.clone()));
        let page = match repository.page(ReplayQuery {
            stream_id: Some(stream_id.to_string()),
            after,
            limit: PROJECTOR_PAGE_LIMIT,
            cutex_session_id: Some(owner.to_string()),
        }) {
            Ok(page) => page,
            Err(ReplayError::CursorExpired { .. }) => {
                let before = self.state.state.clone();
                let owner_cursor = self
                    .state
                    .state
                    .owners
                    .get_mut(owner)
                    .context("activity projector owner cursor disappeared")?;
                owner_cursor.cursor = None;
                owner_cursor.scan_cursor = None;
                if let Err(error) = self.state.save() {
                    self.state.state = before;
                    return Err(error);
                }
                return Ok(());
            }
            Err(error) => return Err(replay_error(error)),
        };
        let page_next_cursor = page.next_cursor;
        let mut scanned_through = None;
        let mut deferred_to_live = false;
        let known_through_sequence = self
            .state
            .state
            .owners
            .get(owner)
            .map(|cursor| {
                cursor
                    .pending
                    .iter()
                    .map(|pending| pending.sequence)
                    .fold(cursor.sequence, u64::max)
            })
            .unwrap_or_default();
        let live_acknowledged_sequence = self
            .state
            .state
            .owners
            .get(owner)
            .map(|cursor| cursor.live_acknowledged_sequence)
            .unwrap_or_default();
        let live_origin_sequence = self.state.state.live_origin_sequence;
        let mut additions = Vec::new();
        for event in page.events {
            if event.sequence <= known_through_sequence {
                scanned_through = Some(event.cursor);
                continue;
            }
            let disposition = if event.sequence > live_origin_sequence && supported_method(&event) {
                if event.sequence > live_acknowledged_sequence {
                    // The global live scanner owns every post-activation fact.
                    // Do not advance the owner scan past a fact that has not yet
                    // reached its live acknowledgement fence, or a fast owner
                    // catch-up pass can mislabel normal activity as recovered.
                    deferred_to_live = true;
                    break;
                }
                MappingDisposition::Skip
            } else {
                self.mapper.map_or_skip(&event)?
            };
            let event_cursor = event.cursor.clone();
            let activity = match disposition {
                MappingDisposition::Project(activity) => Some(Some(*activity)),
                // A Skip is a supported authoritative fact that was
                // intentionally not projected. Keep its ordered checkpoint.
                MappingDisposition::Skip => Some(None),
                // Ignore covers unrelated native/app-server history. The
                // global scan boundary alone accounts for it.
                MappingDisposition::Ignore => None,
            };
            if let Some(activity) = activity {
                additions.push(PendingProjection {
                    stream_id: stream_id.to_string(),
                    cursor: event.cursor,
                    sequence: event.sequence,
                    delivery_class: CutexUiActivityDeliveryClass::CatchUp,
                    delivery: None,
                    recovered: true,
                    activity,
                });
            }
            scanned_through = Some(event_cursor);
        }
        if !deferred_to_live {
            // Owner-filtered replay pages retain a global scan cursor even
            // when the page contains no owner events. Preserve that boundary
            // unless this pass deliberately stopped before a future live fact.
            scanned_through = page_next_cursor;
        }

        let added = !additions.is_empty();
        let (pending_before, scan_cursor_before, changed) = {
            let owner_cursor = self
                .state
                .state
                .owners
                .get_mut(owner)
                .context("activity projector owner cursor disappeared")?;
            let pending_before = added.then(|| owner_cursor.pending.clone());
            let scan_cursor_before = owner_cursor.scan_cursor.clone();
            owner_cursor.pending.extend(additions);
            bound_pending(&mut owner_cursor.pending);
            if let Some(scanned_through) = scanned_through {
                owner_cursor.scan_cursor = Some(scanned_through);
            }
            let changed = added || owner_cursor.scan_cursor != scan_cursor_before;
            (pending_before, scan_cursor_before, changed)
        };
        if changed && !added {
            // Only this owner's scan boundary moved (every new event moves it
            // for every owner).
            return self.state.save_cursor();
        }
        if changed {
            // Persist projected/Skip presentation data and the global scan
            // boundary in one atomic snapshot before transport. If the save
            // fails, restore the in-memory state so project_owner can drain
            // only entries that were already durable.
            if let Err(error) = self.state.save() {
                let owner_cursor = self
                    .state
                    .state
                    .owners
                    .get_mut(owner)
                    .context("activity projector owner cursor disappeared")?;
                if let Some(pending_before) = pending_before {
                    owner_cursor.pending = pending_before;
                }
                owner_cursor.scan_cursor = scan_cursor_before;
                return Err(error);
            }
        }
        Ok(())
    }

    fn drain_owner_queue(
        &mut self,
        stream_id: &str,
        owner: &str,
        delivery_class: CutexUiActivityDeliveryClass,
    ) -> anyhow::Result<()> {
        self.prepare_next_batch(owner, delivery_class)?;
        let mut processed = 0;
        let drained = self.drain_prepared(stream_id, owner, delivery_class, &mut processed);
        // One save for the batch instead of one per entry (each rewrote the
        // whole state). An entry accepted but not yet saved when the process
        // stops is submitted again with the same delivery metadata and comes
        // back Duplicate.
        if processed > 0 {
            let saved = self.state.save();
            drained?;
            return saved;
        }
        drained
    }

    fn drain_prepared(
        &mut self,
        stream_id: &str,
        owner: &str,
        delivery_class: CutexUiActivityDeliveryClass,
        processed: &mut usize,
    ) -> anyhow::Result<()> {
        while *processed < PROJECTOR_DELIVERY_BATCH_LIMIT {
            let Some((pending_index, pending)) =
                self.state.state.owners.get(owner).and_then(|cursor| {
                    cursor
                        .pending
                        .iter()
                        .position(|pending| pending.delivery_class == delivery_class)
                        .map(|index| (index, cursor.pending[index].clone()))
                })
            else {
                return Ok(());
            };
            if let Some(activity) = pending.activity {
                let delivery = pending
                    .delivery
                    .clone()
                    .context("projected activity is missing durable delivery metadata")?;
                let disposition = self.submitter.submit(owner, delivery, activity)?;
                if !matches!(
                    disposition,
                    CutexUiActivityIngestionDisposition::Accepted
                        | CutexUiActivityIngestionDisposition::Duplicate
                        | CutexUiActivityIngestionDisposition::Stale
                ) {
                    unreachable!("all typed ingestion dispositions are terminal");
                }
            }
            let owner_cursor = self
                .state
                .state
                .owners
                .get_mut(owner)
                .context("activity projector owner cursor disappeared")?;
            owner_cursor.pending.remove(pending_index);
            if delivery_class == CutexUiActivityDeliveryClass::Live {
                owner_cursor.live_acknowledged_sequence = pending.sequence;
            }
            if delivery_class == CutexUiActivityDeliveryClass::CatchUp
                && pending.stream_id == stream_id
            {
                owner_cursor.stream_id = Some(stream_id.to_string());
                owner_cursor.cursor = Some(pending.cursor);
                owner_cursor.sequence = pending.sequence;
            }
            // Advance only after typed acceptance/duplicate/stale, or an
            // intentional non-projectable entry. An uncertain response returns
            // above with this durable queue and acknowledgement cursor
            // unchanged. The independent scan cursor must never move backward
            // across already-scanned events belonging to other owners.
            *processed += 1;
        }
        Ok(())
    }

    fn prepare_next_batch(
        &mut self,
        owner: &str,
        delivery_class: CutexUiActivityDeliveryClass,
    ) -> anyhow::Result<()> {
        // Find the batch before copying anything: this runs for every owner
        // twice per pass, and a full state copy (thousands of stuck pending
        // activities on the Linux host) per call kept a core busy, 2026-10-09.
        let positions = self
            .state
            .state
            .owners
            .get(owner)
            .context("activity projector owner cursor disappeared")?
            .pending
            .iter()
            .enumerate()
            .filter(|(_, pending)| {
                pending.delivery_class == delivery_class
                    && pending.activity.is_some()
                    && pending.delivery.is_none()
            })
            .take(PROJECTOR_DELIVERY_BATCH_LIMIT)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let Some(first) = positions.first().copied() else {
            return Ok(());
        };
        let before = self.state.state.clone();
        let owner_cursor = self
            .state
            .state
            .owners
            .get_mut(owner)
            .context("activity projector owner cursor disappeared")?;
        let last = *positions.last().expect("nonempty batch positions");
        let batch_checkpoint = CutexUiActivityCheckpoint {
            stream_id: owner_cursor.pending[last].stream_id.clone(),
            cursor: owner_cursor.pending[last].cursor.clone(),
            sequence: owner_cursor.pending[last].sequence,
        };
        let class_label = match delivery_class {
            CutexUiActivityDeliveryClass::Live => "live",
            CutexUiActivityDeliveryClass::CatchUp => "catch_up",
        };
        let batch_id = format!(
            "{class_label}:{}:{}:{}",
            owner_cursor.pending[first].stream_id,
            owner_cursor.pending[first].sequence,
            owner_cursor.pending[last].sequence
        );
        let batch_size = u32::try_from(positions.len()).context("activity batch is too large")?;
        for (batch_index, position) in positions.into_iter().enumerate() {
            let pending = &mut owner_cursor.pending[position];
            pending.delivery = Some(CutexUiActivityDelivery {
                schema: CutexUiActivityDeliverySchema::V1,
                class: delivery_class,
                recovered: pending.recovered,
                batch_id: batch_id.clone(),
                batch_index: u32::try_from(batch_index)
                    .context("activity batch index is too large")?,
                batch_size,
                source_checkpoint: CutexUiActivityCheckpoint {
                    stream_id: pending.stream_id.clone(),
                    cursor: pending.cursor.clone(),
                    sequence: pending.sequence,
                },
                batch_checkpoint: batch_checkpoint.clone(),
            });
        }
        if let Err(error) = self.state.save() {
            self.state.state = before;
            return Err(error);
        }
        Ok(())
    }
}

fn replay_error(error: ReplayError) -> anyhow::Error {
    anyhow::anyhow!(error.to_string())
}

fn default_projection_state_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("management-v2/activity-projector-state.json"))
}

struct ActivityProjectorLeaderLock {
    file: File,
}

impl ActivityProjectorLeaderLock {
    fn try_acquire(path: &std::path::Path) -> anyhow::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create activity projector lock directory {}",
                    parent.display()
                )
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| {
                format!("failed to open activity projector lock {}", path.display())
            })?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if crate::platform::file_lock::lock_contended(&error) => Ok(None),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to acquire activity projector lock {}",
                    path.display()
                )
            }),
        }
    }
}


impl Drop for ActivityProjectorLeaderLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

const PROJECTOR_FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Retry clock and log de-duplication of the projector loop.
#[derive(Debug, Default)]
struct PassFailure {
    consecutive: u32,
    last_reported: Option<String>,
}

impl PassFailure {
    /// The sleep before the next pass. `report` sees a failure only when its
    /// text differs from the last reported one.
    fn next_delay(&mut self, outcome: Result<(), String>, report: impl FnOnce(&str)) -> Duration {
        match outcome {
            Ok(()) => {
                self.consecutive = 0;
                self.last_reported = None;
                PROJECTOR_POLL_INTERVAL
            }
            Err(detail) => {
                if self.last_reported.as_deref() != Some(detail.as_str()) {
                    report(&detail);
                    self.last_reported = Some(detail);
                }
                self.consecutive = self.consecutive.saturating_add(1);
                let factor = 1u32 << self.consecutive.min(7);
                (PROJECTOR_POLL_INTERVAL * factor).min(PROJECTOR_FAILURE_BACKOFF_MAX)
            }
        }
    }
}

/// Starts a projector contender. The durable file lock elects exactly one
/// cross-process leader; only presentation cursors are mutable here, while the
/// Management v2 repository remains authoritative.
pub fn spawn_activity_projector(
    manager: AppServerRuntimeManager,
    emit_background_diagnostics: bool,
) -> anyhow::Result<()> {
    let state_path = default_projection_state_path()?;
    let lock_path = state_path.with_file_name("activity-projector.lock");
    thread::Builder::new()
        .name("cutex-activity-projector".to_string())
        .spawn(move || {
            let _leader = loop {
                match ActivityProjectorLeaderLock::try_acquire(&lock_path) {
                    Ok(Some(leader)) => break leader,
                    Ok(None) => thread::sleep(PROJECTOR_POLL_INTERVAL),
                    Err(error) => {
                        if emit_background_diagnostics {
                            eprintln!(
                                "\x1b[33mwarning:\x1b[0m Cutex activity projector leadership unavailable: {error:#}"
                            );
                        }
                        thread::sleep(PROJECTOR_POLL_INTERVAL);
                    }
                }
            };
            let mut projector = match ActivityProjector::new(
                state_path,
                manager,
                RegistryParticipantMetadataResolver,
            ) {
                Ok(projector) => projector,
                Err(error) => {
                    if emit_background_diagnostics {
                        eprintln!(
                            "\x1b[33mwarning:\x1b[0m failed to initialize Cutex activity projector: {error:#}"
                        );
                    }
                    return;
                }
            };
            // A failing pass is retried after an exponential delay (250 ms ..
            // 30 s), and the same failure is logged once, not every 250 ms.
            let mut failure = PassFailure::default();
            loop {
                let outcome = match management_v2_repository() {
                    Ok(repository) => projector
                        .run_once(repository)
                        .map_err(|error| format!("pass failed: {error:#}")),
                    Err(error) => Err(format!("repository unavailable: {error:#}")),
                };
                let delay = failure.next_delay(outcome, |detail| {
                    eprintln!("\x1b[33mwarning:\x1b[0m Cutex activity projector {detail}")
                });
                thread::sleep(delay);
            }
        })
        .context("failed to spawn Cutex activity projector")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::model::{
        AppServerSchema, AppServerSchemaChannel, CutexMessage, EventCorrelation, EventSource,
        NativeMessage, NativeMessageKind, PendingEvent,
    };
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("cutex-activity-projector-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn projector_leader_lock_fences_concurrent_processes_and_allows_takeover() {
        let temp = TestDir::new();
        let path = temp.path().join("activity-projector.lock");
        let first = ActivityProjectorLeaderLock::try_acquire(&path)
            .unwrap()
            .expect("first leader");
        assert!(ActivityProjectorLeaderLock::try_acquire(&path)
            .unwrap()
            .is_none());
        drop(first);
        assert!(ActivityProjectorLeaderLock::try_acquire(&path)
            .unwrap()
            .is_some());
    }

    #[derive(Clone, Copy)]
    struct FakeParticipants;

    impl ParticipantMetadataResolver for FakeParticipants {
        fn resolve(
            &self,
            cutex_session_id: &str,
        ) -> super::super::commands::ParticipantPresentationMetadata {
            super::super::commands::ParticipantPresentationMetadata {
                display_name: Some(format!("display:{cutex_session_id}")),
                cutex_session_id: Some(cutex_session_id.to_string()),
                profile: Some("test-profile".to_string()),
                model: Some("test-model".to_string()),
                reasoning: Some("high".to_string()),
                role: None,
                runtime_backend: Some("host".to_string()),
            }
        }
    }

    #[derive(Default)]
    /// Hermetic fake app-server ingestion endpoint. It records only the typed
    /// UI lane payload and can inject transport/disposition outcomes.
    struct FakeAppServer {
        submissions: Mutex<Vec<(String, CutexUiActivity)>>,
        deliveries: Mutex<Vec<CutexUiActivityDelivery>>,
        outcomes: Mutex<Vec<anyhow::Result<CutexUiActivityIngestionDisposition>>>,
    }

    impl ActivitySubmitter for Arc<FakeAppServer> {
        fn submit(
            &self,
            owner: &str,
            delivery: CutexUiActivityDelivery,
            activity: CutexUiActivity,
        ) -> anyhow::Result<CutexUiActivityIngestionDisposition> {
            let outcome = {
                let mut outcomes = self.outcomes.lock().unwrap();
                if outcomes.is_empty() {
                    Ok(CutexUiActivityIngestionDisposition::Accepted)
                } else {
                    outcomes.remove(0)
                }
            };
            if outcome.is_ok() {
                self.deliveries.lock().unwrap().push(delivery);
                self.submissions
                    .lock()
                    .unwrap()
                    .push((owner.to_string(), activity));
            }
            outcome
        }
    }

    fn test_delivery(class: CutexUiActivityDeliveryClass) -> CutexUiActivityDelivery {
        CutexUiActivityDelivery {
            schema: CutexUiActivityDeliverySchema::V1,
            class,
            recovered: class == CutexUiActivityDeliveryClass::CatchUp,
            batch_id: "catch_up:stream-1:1:1".to_string(),
            batch_index: 0,
            batch_size: 1,
            source_checkpoint: CutexUiActivityCheckpoint {
                stream_id: "stream-1".to_string(),
                cursor: "cursor-1".to_string(),
                sequence: 1,
            },
            batch_checkpoint: CutexUiActivityCheckpoint {
                stream_id: "stream-1".to_string(),
                cursor: "cursor-1".to_string(),
                sequence: 1,
            },
        }
    }

    fn repository(temp: &TestDir) -> EventRepository {
        EventRepository::open(temp.path().join("repository"), "host-1").unwrap()
    }

    fn append(
        repository: &EventRepository,
        owner: &str,
        method: &str,
        params: serde_json::Value,
    ) -> EventEnvelope {
        repository
            .append(PendingEvent {
                cutex_session_id: owner.to_string(),
                host_id: "host-1".to_string(),
                source: EventSource::Cutex,
                schema: None,
                correlation: EventCorrelation::default(),
                native: None,
                cutex: Some(CutexMessage {
                    method: method.to_string(),
                    params,
                }),
            })
            .unwrap()
    }

    fn message_params(content: &str) -> serde_json::Value {
        serde_json::json!({
            "messageId": "message-1",
            "fromCutexSessionId": "cutex.director",
            "toCutexSessionId": "cutex.worker",
            "fromRuntimeAgentId": null,
            "toRuntimeAgentId": null,
            "deliveryMode": "soon",
            "content": content,
            "sentAt": "2026-08-28T01:02:03Z"
        })
    }

    fn message_params_with_id(message_id: &str, content: &str) -> serde_json::Value {
        let mut params = message_params(content);
        params["messageId"] = serde_json::json!(message_id);
        params
    }

    fn append_other_owner_gap(repository: &EventRepository, index: usize) -> EventEnvelope {
        append(
            repository,
            "cutex.other",
            "cutex/test/ignored",
            serde_json::json!({ "index": index }),
        )
    }

    fn append_ignored_native(
        repository: &EventRepository,
        owner: &str,
        index: usize,
    ) -> EventEnvelope {
        repository
            .append(PendingEvent {
                cutex_session_id: owner.to_string(),
                host_id: "host-1".to_string(),
                source: EventSource::AppServer,
                schema: Some(AppServerSchema {
                    protocol: "codex-app-server".to_string(),
                    major_version: 2,
                    version: "test".to_string(),
                    sha256: "a".repeat(64),
                    channel: AppServerSchemaChannel::Experimental,
                    capabilities: serde_json::json!({}),
                    extensions: Vec::new(),
                }),
                correlation: EventCorrelation {
                    runtime_generation: Some(1),
                    ..EventCorrelation::default()
                },
                native: Some(NativeMessage {
                    kind: NativeMessageKind::Notification,
                    message: serde_json::json!({
                        "method": "item/started",
                        "params": { "index": index }
                    }),
                }),
                cutex: None,
            })
            .unwrap_or_else(|error| panic!("append ignored native event {index}: {error:#}"))
    }

    fn failure_params(target: Option<&str>, operation: &str) -> serde_json::Value {
        serde_json::json!({
            "schema": "cutex/agent-management-failure/v1",
            "event_id": "agent-management:action-1:failure",
            "action_id": "action-1",
            "project_id": "project-1",
            "operation": operation,
            "code": "owner_action_required",
            "detail": "runtime unavailable",
            "routing_status": "routable",
            "route_to_director_session": "cutex.director",
            "target_cutex_session_id": target,
            "created_at": "2026-08-28T01:02:03Z"
        })
    }

    fn phase_params(
        phase: &str,
        phase_sequence: u64,
        owner: &str,
        authority_epoch: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema": AGENT_MANAGEMENT_PHASE_TRANSITION_SCHEMA,
            "phase_event_id": format!("agent-management:rotate:phase:{phase_sequence}"),
            "action_id": "rotate",
            "project_id": "project-1",
            "operation": "director_rotate",
            "phase": phase,
            "phase_sequence": phase_sequence,
            "committed_at": "2026-08-28T01:02:03Z",
            "primary_presentation_target_cutex_session_id": owner,
            "primary_presentation_target_metadata": {
                "displayName": format!("display:{owner}"),
                "cutexSessionId": owner,
                "profile": null,
                "model": null,
                "reasoning": null,
                "role": null,
                "runtimeBackend": "host"
            },
            "predecessor_cutex_session_id": "cutex.director-old",
            "predecessor_metadata": {
                "displayName": "Old Director",
                "cutexSessionId": "cutex.director-old",
                "profile": null,
                "model": null,
                "reasoning": null,
                "role": null,
                "runtimeBackend": "host"
            },
            "successor_cutex_session_id": "cutex.director-new",
            "successor_metadata": {
                "displayName": "New Director",
                "cutexSessionId": "cutex.director-new",
                "profile": null,
                "model": null,
                "reasoning": null,
                "role": null,
                "runtimeBackend": "host"
            },
            "replace_policy": null,
            "rotation_mode": "close_predecessor_then_create_with_message",
            "authority_epoch": authority_epoch
        })
    }

    fn lifecycle_params(operation: &str, result_kind: &str) -> serde_json::Value {
        let agent = serde_json::json!({
            "project_id": "project-1",
            "created_by_director_session": "cutex.director",
            "cutex_session_id": "cutex.worker",
            "native_session_id": "native-worker",
            "spec": {
                "name": "Worker",
                "cwd": "/tmp/worker",
                "profile": "worker",
                "runtime_backend": "native",
                "model": "gpt-5",
                "reasoning": "medium",
                "permissions": "workspace-write",
                "approval_policy": "never",
                "sandbox_mode": "workspace-write",
                "groups": ["project-1"],
                "expose_to_im": false,
                "pin": false
            },
            "created_at": "2026-08-28T01:00:00Z",
            "retired_at": null
        });
        let observation = serde_json::json!({
            "cutex_session_id": "cutex.worker",
            "native_session_id": "native-worker",
            "active": true,
            "cwd": "/tmp/worker",
            "profile": "worker",
            "runtime_backend": "native",
            "model": "gpt-5",
            "reasoning": "medium",
            "permissions": "workspace-write",
            "approval_policy": "never",
            "sandbox_mode": "workspace-write",
            "groups": ["project-1"],
            "runtime_generation": 7,
            "runtime_agent_ids": [],
            "app_server_runtime": true,
            "agent_bus_endpoint_ids": []
        });
        let result = match result_kind {
            "created" => serde_json::json!({
                "kind": "created", "agent": agent, "observation": observation,
                "message_id": null
            }),
            "lifecycle" => serde_json::json!({
                "kind": "lifecycle", "agent": agent, "observation": observation
            }),
            _ => unreachable!(),
        };
        serde_json::json!({
            "schema": "cutex/agent-management-receipt/v1",
            "action_id": format!("action-{operation}"),
            "request_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "operation": operation,
            "project_id": "project-1",
            "completed_at": "2026-08-28T01:02:03Z",
            "result": result
        })
    }

    fn query_params() -> serde_json::Value {
        serde_json::json!({
            "schema": "cutex/agent-management-receipt/v1",
            "action_id": "action-query",
            "request_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "operation": "query_managed",
            "project_id": "project-1",
            "completed_at": "2026-08-28T01:02:03Z",
            "result": {
                "kind": "query_managed",
                "authority": {
                    "project_id": "project-1",
                    "authorized_director_session": "cutex.director",
                    "authority_epoch": 1,
                    "updated_at": "2026-08-28T01:00:00Z"
                },
                "agents": []
            }
        })
    }

    fn projector(
        temp: &TestDir,
        submitter: Arc<FakeAppServer>,
    ) -> ActivityProjector<Arc<FakeAppServer>, FakeParticipants> {
        ActivityProjector::new(temp.path().join("projector.json"), submitter, FakeParticipants)
            .unwrap()
    }

    #[test]
    fn existing_target_failure_projects_and_pre_identity_failure_skips() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        append(
            &repository,
            "cutex.director",
            ACTION_FAILED,
            failure_params(Some("cutex.worker"), "restart"),
        );
        append(
            &repository,
            "cutex.director",
            ACTION_FAILED,
            failure_params(None, "create"),
        );
        let submitter = Arc::new(FakeAppServer::default());
        projector(&temp, submitter.clone())
            .run_once(&repository)
            .unwrap();
        let submissions = submitter.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        assert!(matches!(
            &submissions[0].1,
            CutexUiActivity::ManagedAgentActivity(item)
                if item.managed_agent_id == "cutex.worker"
                    && item.operation == ManagedAgentOperation::Restart
                    && item.status == ManagedAgentActivityStatus::Failed
                    && item.action_id.is_none()
                    && item.phase_event_id.is_none()
                    && item.phase.is_none()
        ));
    }

    #[test]
    fn successful_lifecycle_operations_project_exact_managed_identity_and_query_skips() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        append(
            &repository,
            "cutex.director",
            ACTION_COMPLETED,
            lifecycle_params("create", "created"),
        );
        append(
            &repository,
            "cutex.director",
            ACTION_COMPLETED,
            lifecycle_params("online", "lifecycle"),
        );
        append(
            &repository,
            "cutex.director",
            ACTION_COMPLETED,
            query_params(),
        );
        let submitter = Arc::new(FakeAppServer::default());
        projector(&temp, submitter.clone())
            .run_once(&repository)
            .unwrap();
        let submissions = submitter.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 2);
        assert!(matches!(
            &submissions[0].1,
            CutexUiActivity::ManagedAgentActivity(item)
                if item.managed_agent_id == "cutex.worker"
                    && item.operation == ManagedAgentOperation::Create
                    && item.runtime_generation == Some(7)
                    && item.action_id.is_none()
                    && item.phase_event_id.is_none()
                    && item.phase.is_none()
        ));
        assert!(matches!(
            &submissions[1].1,
            CutexUiActivity::ManagedAgentActivity(item)
                if item.managed_agent_id == "cutex.worker"
                    && item.operation == ManagedAgentOperation::Online
                    && item.action_id.is_none()
                    && item.phase_event_id.is_none()
                    && item.phase.is_none()
        ));
        assert_eq!(
            managed_operation(AgentOperationKind::Offline),
            Some(ManagedAgentOperation::Offline)
        );
        assert_eq!(
            managed_operation(AgentOperationKind::Restart),
            Some(ManagedAgentOperation::Restart)
        );
        assert_eq!(
            managed_operation(AgentOperationKind::Replace),
            Some(ManagedAgentOperation::Replace)
        );
        assert_eq!(
            managed_operation(AgentOperationKind::Close),
            Some(ManagedAgentOperation::Close)
        );
        assert_eq!(
            managed_operation(AgentOperationKind::DirectorRotate),
            Some(ManagedAgentOperation::DirectorRotate)
        );
        assert_eq!(managed_operation(AgentOperationKind::QueryManaged), None);
    }

    #[test]
    fn early_create_phase_projects_requested_agent_name_not_presentation_owner() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let mut params = phase_params("prepared", 1, "cutex.director", 1);
        params["operation"] = serde_json::json!("create");
        params["action_id"] = serde_json::json!("create-literature-agent");
        params["phase_event_id"] =
            serde_json::json!("agent-management:create-literature-agent:phase:1");
        params["subject_cutex_session_id"] = serde_json::Value::Null;
        params["subject_agent_name"] = serde_json::json!("demo-literature-brief-r1");
        params["predecessor_cutex_session_id"] = serde_json::Value::Null;
        params["predecessor_metadata"] = serde_json::Value::Null;
        params["successor_cutex_session_id"] = serde_json::Value::Null;
        params["successor_metadata"] = serde_json::Value::Null;
        params["rotation_mode"] = serde_json::Value::Null;
        append(
            &repository,
            "cutex.director",
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
            params,
        );
        let submitter = Arc::new(FakeAppServer::default());

        projector(&temp, submitter.clone())
            .run_once(&repository)
            .unwrap();

        let submissions = submitter.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        let CutexUiActivity::ManagedAgentActivity(activity) = &submissions[0].1 else {
            panic!("expected managed Agent activity")
        };
        assert_eq!(
            activity.managed_agent_name.as_deref(),
            Some("demo-literature-brief-r1")
        );
        assert_eq!(activity.managed_agent_id, "pending:create-literature-agent");
        assert_ne!(activity.managed_agent_id, "cutex.director");
    }

    #[test]
    fn early_rotation_phase_populates_successor_name_before_session_capture() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let mut params = phase_params("prepared", 1, "cutex.director-old", 7);
        params["subject_cutex_session_id"] = serde_json::Value::Null;
        params["subject_agent_name"] = serde_json::json!("cutex-director-r12");
        params["successor_cutex_session_id"] = serde_json::Value::Null;
        params["successor_metadata"] = serde_json::Value::Null;
        append(
            &repository,
            "cutex.director-old",
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
            params,
        );
        let submitter = Arc::new(FakeAppServer::default());

        projector(&temp, submitter.clone())
            .run_once(&repository)
            .unwrap();

        let submissions = submitter.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        let CutexUiActivity::ManagedAgentActivity(activity) = &submissions[0].1 else {
            panic!("expected managed Agent phase")
        };
        assert_eq!(activity.phase, Some(ManagedAgentActionPhase::Prepared));
        assert_eq!(
            activity.managed_agent_name.as_deref(),
            Some("cutex-director-r12")
        );
        assert_eq!(
            activity.successor_agent_name.as_deref(),
            Some("cutex-director-r12")
        );
        assert!(activity.successor_agent_id.is_none());
    }

    #[test]
    fn rotation_completion_receipt_does_not_bypass_phase_presentation() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let created = lifecycle_params("create", "created");
        let successor = created["result"]["agent"].clone();
        let observation = created["result"]["observation"].clone();
        append(
            &repository,
            "cutex.director-old",
            ACTION_COMPLETED,
            serde_json::json!({
                "schema": "cutex/agent-management-receipt/v1",
                "action_id": "rotate",
                "request_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "operation": "director_rotate",
                "project_id": "project-1",
                "completed_at": "2026-09-01T00:00:00Z",
                "result": {
                    "kind": "director_rotated",
                    "predecessor_cutex_session_id": "cutex.director-old",
                    "successor": successor,
                    "observation": observation,
                    "authority": {
                        "project_id": "project-1",
                        "authorized_director_session": "cutex.worker",
                        "authority_epoch": 8,
                        "updated_at": "2026-09-01T00:00:00Z"
                    },
                    "message_id": null
                }
            }),
        );
        let submitter = Arc::new(FakeAppServer::default());

        projector(&temp, submitter.clone())
            .run_once(&repository)
            .unwrap();

        assert!(submitter.submissions.lock().unwrap().is_empty());
    }

    #[test]
    fn rotation_phase_projection_switches_exact_audience_and_stays_ui_only() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let mut closing_params = phase_params("predecessor_closing", 2, "cutex.director-old", 7);
        closing_params["primary_presentation_target_metadata"] = serde_json::Value::Null;
        append(
            &repository,
            "cutex.director-old",
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
            closing_params,
        );
        append(
            &repository,
            "cutex.director-new",
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
            phase_params("successor_ready", 14, "cutex.director-new", 8),
        );
        let submitter = Arc::new(FakeAppServer::default());
        projector(&temp, submitter.clone())
            .run_once(&repository)
            .unwrap();
        let submissions = submitter.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 2);
        let closing_submission = submissions
            .iter()
            .find(|(owner, _)| owner == "cutex.director-old")
            .unwrap();
        let ready_submission = submissions
            .iter()
            .find(|(owner, _)| owner == "cutex.director-new")
            .unwrap();
        let CutexUiActivity::ManagedAgentActivity(closing) = &closing_submission.1 else {
            panic!("expected managed Agent phase")
        };
        assert_eq!(closing.id, "rotate");
        assert_eq!(closing.managed_agent_name.as_deref(), Some("Old Director"));
        assert_eq!(
            closing.phase,
            Some(ManagedAgentActionPhase::PredecessorClosing)
        );
        assert_eq!(
            closing.predecessor_agent_name.as_deref(),
            Some("Old Director")
        );
        assert_eq!(
            closing.successor_agent_name.as_deref(),
            Some("New Director")
        );
        assert_eq!(closing.authority_epoch, Some(7));
        let CutexUiActivity::ManagedAgentActivity(ready) = &ready_submission.1 else {
            panic!("expected managed Agent phase")
        };
        assert_eq!(ready.phase, Some(ManagedAgentActionPhase::SuccessorReady));
        assert_eq!(ready.managed_agent_id, "cutex.director-new");
        assert_eq!(ready.authority_epoch, Some(8));
        let wire = serde_json::to_value(ThreadCutexActivityParams {
            thread_id: "thread-successor".to_string(),
            delivery: test_delivery(CutexUiActivityDeliveryClass::CatchUp),
            activity: ready_submission.1.clone(),
        })
        .unwrap();
        assert!(wire.get("turnId").is_none());
        let encoded = serde_json::to_string(&wire).unwrap();
        for forbidden in ["thread/inject_items", "ResponseItem", "parentThreadId"] {
            assert!(!encoded.contains(forbidden));
        }
    }

    #[test]
    fn expired_projector_cursors_rewind_to_retained_boundary_without_duplicate_delivery() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params("retained activity"),
        );
        let metadata = repository.stream_metadata().unwrap();
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());
        projector.state.state.stream_id = Some(metadata.stream_id.clone());
        projector.state.state.live_initialized = true;
        projector.state.state.live_cursor = Some("c2:expired-live".to_string());
        projector.state.state.discovery_cursor = Some("c2:expired-discovery".to_string());
        projector.state.state.owners.insert(
            "cutex.director".to_string(),
            OwnerProjectionCursor {
                stream_id: Some(metadata.stream_id),
                cursor: Some("c2:expired-owner".to_string()),
                scan_cursor: Some("c2:expired-owner-scan".to_string()),
                ..Default::default()
            },
        );
        projector.state.save().unwrap();

        // The first pass durably rewinds each expired scan. The second pass
        // replays retained records, with live/owner sequence fences preventing
        // the same activity from being submitted twice.
        projector.run_once(&repository).unwrap();
        projector.run_once(&repository).unwrap();

        let submissions = submitter.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        assert!(matches!(
            &submissions[0].1,
            CutexUiActivity::OutboundInterAgentMessage(item)
                if item.content_preview.as_deref() == Some("retained activity")
        ));
        assert_eq!(projector.state.state.live_sequence, 1);
        let owner = projector
            .state
            .state
            .owners
            .get("cutex.director")
            .expect("owner cursor");
        assert_eq!(owner.sequence, 1);
        assert_eq!(owner.live_acknowledged_sequence, 1);
        assert!(owner.pending.is_empty());
    }

    #[test]
    fn owner_catch_up_cannot_overtake_global_live_classification() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let mut boundary = None;
        for index in 0..PROJECTOR_PAGE_LIMIT {
            boundary = Some(append_ignored_native(&repository, "cutex.other", index));
        }
        append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params("must stay live"),
        );
        let metadata = repository.stream_metadata().unwrap();
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());
        projector.state.state.stream_id = Some(metadata.stream_id.clone());
        projector.state.state.live_initialized = true;
        projector.state.state.live_origin_sequence = 0;
        projector.state.state.live_sequence = 0;
        projector.state.state.live_cursor = None;
        projector.state.state.owners.insert(
            "cutex.director".to_string(),
            OwnerProjectionCursor {
                stream_id: Some(metadata.stream_id),
                // Simulate an owner catch-up cursor that was already ahead of
                // the recovering global live cursor.
                scan_cursor: boundary.map(|event| event.cursor),
                ..Default::default()
            },
        );
        projector.state.save().unwrap();

        projector.run_once(&repository).unwrap();
        assert!(submitter.submissions.lock().unwrap().is_empty());
        projector.run_once(&repository).unwrap();

        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
        let deliveries = submitter.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].class, CutexUiActivityDeliveryClass::Live);
        assert!(!deliveries[0].recovered);
    }

    /// r219 rewrite of the former Task lifecycle variant onto Agent Bus
    /// `messageSent` facts: live records preempt a large catch-up backlog,
    /// survive an uncertain transport and a restart, and are never delivered
    /// twice when catch-up reaches them.
    #[test]
    fn live_activity_preempts_large_catch_up_and_recovers_without_duplicate() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let historical_count = PROJECTOR_DELIVERY_BATCH_LIMIT * 3;
        for index in 0..historical_count {
            append(
                &repository,
                "cutex.director",
                MESSAGE_SENT,
                message_params_with_id(&format!("historical-{index:03}"), "historical"),
            );
        }

        let submitter = Arc::new(FakeAppServer::default());
        let mut first = projector(&temp, submitter.clone());
        first.run_once(&repository).unwrap();
        assert_eq!(
            submitter.submissions.lock().unwrap().len(),
            PROJECTOR_DELIVERY_BATCH_LIMIT,
            "one bounded catch-up batch is delivered per pass"
        );

        let live = |index: usize, second: u32| {
            let mut params = message_params_with_id(&format!("live-{index}"), "live");
            params["sentAt"] = serde_json::json!(format!("2026-08-28T01:02:{second:02}Z"));
            append(&repository, "cutex.director", MESSAGE_SENT, params)
        };
        let first_live = live(0, 3);
        let second_live = live(1, 4);
        let third_live = live(2, 5);

        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("uncertain live transport")));
        first.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 32);
        let durable_after_uncertain: ProjectionState =
            serde_json::from_slice(&fs::read(temp.path().join("projector.json")).unwrap()).unwrap();
        let owner = &durable_after_uncertain.owners["cutex.director"];
        assert_eq!(owner.sequence, PROJECTOR_DELIVERY_BATCH_LIMIT as u64);
        assert_eq!(
            owner
                .pending
                .iter()
                .filter(|pending| pending.delivery_class == CutexUiActivityDeliveryClass::Live)
                .count(),
            3,
            "uncertain live records remain durable and unacknowledged"
        );
        drop(first);

        let mut restarted = projector(&temp, submitter.clone());
        restarted.run_once(&repository).unwrap();
        let submissions = submitter.submissions.lock().unwrap().clone();
        let deliveries = submitter.deliveries.lock().unwrap().clone();
        let live_activities = &submissions[32..35];
        let live_deliveries = &deliveries[32..35];
        for ((_, activity), expected) in live_activities.iter().zip(["live-0", "live-1", "live-2"]) {
            assert!(matches!(activity,
                CutexUiActivity::OutboundInterAgentMessage(item) if item.id == expected
            ));
        }
        assert_eq!(
            live_activities
                .iter()
                .map(|(_, activity)| match activity {
                    CutexUiActivity::OutboundInterAgentMessage(item) => item.occurred_at_ms,
                    _ => unreachable!("checked outbound message activity"),
                })
                .collect::<Vec<_>>(),
            vec![
                timestamp_ms("2026-08-28T01:02:03Z").unwrap(),
                timestamp_ms("2026-08-28T01:02:04Z").unwrap(),
                timestamp_ms("2026-08-28T01:02:05Z").unwrap(),
            ]
        );
        assert_eq!(
            live_deliveries
                .iter()
                .map(|delivery| delivery.source_checkpoint.sequence)
                .collect::<Vec<_>>(),
            vec![first_live.sequence, second_live.sequence, third_live.sequence]
        );
        assert!(live_deliveries.iter().all(|delivery| {
            delivery.class == CutexUiActivityDeliveryClass::Live
                && delivery.recovered
                && delivery.batch_size == 3
                && delivery.batch_checkpoint.sequence == third_live.sequence
        }));
        assert_eq!(
            live_deliveries
                .iter()
                .map(|delivery| delivery.batch_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(live_deliveries
            .windows(2)
            .all(|pair| pair[0].batch_id == pair[1].batch_id));

        for _ in 0..3 {
            restarted.run_once(&repository).unwrap();
        }
        let submissions = submitter.submissions.lock().unwrap();
        let deliveries = submitter.deliveries.lock().unwrap();
        assert_eq!(submissions.len(), historical_count + 3);
        assert_eq!(
            submissions
                .iter()
                .filter(|(_, activity)| matches!(activity,
                    CutexUiActivity::OutboundInterAgentMessage(item) if item.id.starts_with("live-")
                ))
                .count(),
            3,
            "catch-up reaches live cursors as ordered acknowledgements, not duplicate cards"
        );
        let catch_up_sequences = deliveries
            .iter()
            .filter(|delivery| delivery.class == CutexUiActivityDeliveryClass::CatchUp)
            .map(|delivery| delivery.source_checkpoint.sequence)
            .collect::<Vec<_>>();
        assert!(catch_up_sequences.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(deliveries
            .iter()
            .filter(|delivery| delivery.class == CutexUiActivityDeliveryClass::CatchUp)
            .all(|delivery| delivery.recovered && delivery.batch_size <= 32));
        let owner = &restarted.state.state.owners["cutex.director"];
        assert!(owner.pending.is_empty());
        assert_eq!(owner.cursor.as_deref(), Some(third_live.cursor.as_str()));

        let wire = serde_json::to_value(ThreadCutexActivityParams {
            thread_id: "thread-director".to_string(),
            delivery: live_deliveries[0].clone(),
            activity: live_activities[0].1.clone(),
        })
        .unwrap();
        assert!(wire.get("turnId").is_none());
        assert_eq!(wire["delivery"]["class"], "live");
        assert_eq!(wire["delivery"]["recovered"], true);
        let mut invalid_schema = wire.clone();
        invalid_schema["delivery"]["schema"] = serde_json::json!("cutex/ui-activity-delivery/v2");
        assert!(serde_json::from_value::<ThreadCutexActivityParams>(invalid_schema).is_err());
        let encoded = serde_json::to_string(&wire).unwrap();
        for forbidden in ["modelContext", "thread/inject_items", "ResponseItem"] {
            assert!(!encoded.contains(forbidden));
        }
    }

    /// r219: a projector state written before the Task Service was removed may
    /// hold pending `taskAssignmentActivity` / `taskWatchdogActivity` entries.
    /// They load as intentional skips (`activity: None`) so the state stays
    /// readable and ordering is kept; Task facts still in the stream are
    /// ignored; any other unknown activity type is still rejected.
    #[test]
    fn legacy_task_pending_entries_load_as_skips_and_task_facts_are_ignored() {
        let temp = TestDir::new();
        let pending = |sequence: u64, activity: serde_json::Value| {
            serde_json::json!({
                "streamId": "stream-1",
                "cursor": format!("cursor-{sequence}"),
                "sequence": sequence,
                "deliveryClass": "catch_up",
                "delivery": null,
                "recovered": false,
                "activity": activity
            })
        };
        let outbound = serde_json::to_value(CutexUiActivity::OutboundInterAgentMessage(
            OutboundInterAgentMessageItem {
                id: "message-1".to_string(),
                event_id: "event-3".to_string(),
                sequence: 3,
                occurred_at_ms: 1,
                sender_agent_id: "cutex.director".to_string(),
                sender_agent_name: None,
                sender_metadata: None,
                recipient_agent_id: "cutex.worker".to_string(),
                recipient_agent_name: None,
                recipient_metadata: None,
                other_recipient_agent_ids: Vec::new(),
                delivery_mode: AgentDeliveryMode::Soon,
                status: OutboundInterAgentMessageStatus::Sent,
                content_preview: Some("hello".to_string()),
                detail: None,
            },
        ))
        .unwrap();
        let state = serde_json::json!({
            "version": PROJECTION_STATE_VERSION,
            "streamId": "stream-1",
            "discoveryCursor": null,
            "owners": {
                "cutex.director": {
                    "streamId": "stream-1",
                    "cursor": null,
                    "sequence": 0,
                    "pending": [
                        pending(1, serde_json::json!({
                            "type": "taskAssignmentActivity",
                            "id": "assignment-1",
                            "status": "committed"
                        })),
                        pending(2, serde_json::json!({
                            "type": "taskWatchdogActivity",
                            "id": "episode-1",
                            "stage": "first_stale"
                        })),
                        pending(3, outbound.clone()),
                        pending(4, serde_json::Value::Null),
                    ]
                }
            }
        });
        let path = temp.path().join("projector.json");
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        let store = ProjectionStateStore::open(path.clone()).unwrap();
        let loaded = &store.state.owners["cutex.director"].pending;
        assert_eq!(
            loaded.iter().map(|entry| entry.sequence).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(loaded[0].activity, None);
        assert_eq!(loaded[1].activity, None);
        assert_eq!(
            loaded[2].activity,
            Some(serde_json::from_value::<CutexUiActivity>(outbound).unwrap())
        );
        assert_eq!(loaded[3].activity, None);
        // Reopening the rewritten state (open() saves the recovered flags)
        // still works and keeps the skips.
        let reopened = ProjectionStateStore::open(path.clone()).unwrap();
        assert_eq!(reopened.state.owners["cutex.director"].pending.len(), 4);
        assert!(reopened.state.owners["cutex.director"].pending[0].activity.is_none());

        let mut unknown = state.clone();
        unknown["owners"]["cutex.director"]["pending"][0]["activity"]["type"] =
            serde_json::json!("somethingElse");
        fs::write(&path, serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert!(ProjectionStateStore::open(path).is_err());

        let repository = repository(&temp);
        let task_fact = append(
            &repository,
            "cutex.director",
            "cutex/taskService/assignmentCommitted",
            serde_json::json!({"legacy": true}),
        );
        let mapper = ActivityMapper::new(FakeParticipants);
        assert_eq!(mapper.map(&task_fact).unwrap(), MappingDisposition::Ignore);
        assert!(!supported_method(&task_fact));
    }

    #[test]
    fn outbound_projection_is_bounded_and_has_no_turn_or_model_injection() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params(&"x".repeat(PREVIEW_LIMIT + 50)),
        );
        let submitter = Arc::new(FakeAppServer::default());
        projector(&temp, submitter.clone())
            .run_once(&repository)
            .unwrap();
        let submissions = submitter.submissions.lock().unwrap();
        let activity = &submissions[0].1;
        assert!(matches!(
            activity,
            CutexUiActivity::OutboundInterAgentMessage(item)
                if item.content_preview.as_ref().unwrap().chars().count() == PREVIEW_LIMIT
                    && item.delivery_mode == AgentDeliveryMode::Soon
        ));
        let params = ThreadCutexActivityParams {
            thread_id: "thread-1".to_string(),
            delivery: test_delivery(CutexUiActivityDeliveryClass::CatchUp),
            activity: activity.clone(),
        };
        let json = serde_json::to_value(params).unwrap();
        assert!(json.get("turnId").is_none());
        let encoded = json.to_string();
        assert!(!encoded.contains("inter_agent_message"));
        assert!(!encoded.contains("inject_items"));
        assert!(!encoded.contains("ResponseItem"));
    }

    #[test]
    fn owner_scan_advances_across_first_global_page_with_zero_owner_events() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let mut first_page_boundary = None;
        for index in 0..PROJECTOR_PAGE_LIMIT {
            first_page_boundary = Some(append_other_owner_gap(&repository, index));
        }
        let target = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-after-empty-page", "after empty page"),
        );
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());

        projector.run_once(&repository).unwrap();
        assert!(!projector.state.state.owners.contains_key("cutex.director"));
        projector.run_once(&repository).unwrap();
        let owner = &projector.state.state.owners["cutex.director"];
        assert_eq!(
            owner.scan_cursor.as_deref(),
            Some(first_page_boundary.unwrap().cursor.as_str()),
            "an owner-filtered empty page must persist the global next cursor"
        );
        assert!(owner.cursor.is_none());
        assert!(owner.pending.is_empty());
        assert!(submitter.submissions.lock().unwrap().is_empty());

        projector.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
        assert_eq!(
            projector.state.state.owners["cutex.director"]
                .cursor
                .as_deref(),
            Some(target.cursor.as_str())
        );
    }

    #[test]
    fn ignored_native_volume_crosses_max_pages_without_pending_or_submission() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let projected = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-before-native-volume", "before native volume"),
        );
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());
        projector.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);

        let ignored_count = PROJECTOR_PAGE_LIMIT * 2 + 37;
        let mut last_ignored = None;
        for index in 0..ignored_count {
            last_ignored = Some(append_ignored_native(&repository, "cutex.director", index));
        }
        for _ in 0..ignored_count.div_ceil(PROJECTOR_PAGE_LIMIT) {
            projector.run_once(&repository).unwrap();
            let owner = &projector.state.state.owners["cutex.director"];
            assert!(owner.pending.is_empty());
            assert_eq!(owner.cursor.as_deref(), Some(projected.cursor.as_str()));
            assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
        }
        assert_eq!(
            projector.state.state.owners["cutex.director"]
                .scan_cursor
                .as_deref(),
            Some(last_ignored.unwrap().cursor.as_str())
        );
    }

    #[test]
    fn projected_and_trailing_ignored_share_one_crash_safe_page_snapshot() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let projected = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-batched-before-ignored", "batched"),
        );
        let mut trailing = None;
        for index in 0..32 {
            trailing = Some(append_ignored_native(&repository, "cutex.director", index));
        }
        let submitter = Arc::new(FakeAppServer::default());
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("uncertain after durable batch")));
        let mut projector = projector(&temp, submitter.clone());
        projector.run_once(&repository).unwrap();

        let persisted: ProjectionState =
            serde_json::from_slice(&fs::read(temp.path().join("projector.json")).unwrap()).unwrap();
        let owner = &persisted.owners["cutex.director"];
        assert!(owner.cursor.is_none());
        assert_eq!(owner.pending.len(), 1);
        assert_eq!(owner.pending[0].cursor, projected.cursor);
        assert!(owner.pending[0].activity.is_some());
        assert_eq!(
            owner.scan_cursor.as_deref(),
            Some(trailing.unwrap().cursor.as_str()),
            "projected pending and trailing Ignore boundary must share the durable page snapshot"
        );
        assert!(submitter.submissions.lock().unwrap().is_empty());
    }

    #[test]
    fn acknowledged_owner_cursor_does_not_rewind_scan_across_other_owner_gap() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let first = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-before-gap", "before gap"),
        );
        let mut first_page_boundary = None;
        for index in 0..PROJECTOR_PAGE_LIMIT - 1 {
            first_page_boundary = Some(append_other_owner_gap(&repository, index));
        }
        let mut second_page_boundary = None;
        for index in 0..PROJECTOR_PAGE_LIMIT {
            second_page_boundary = Some(append_other_owner_gap(
                &repository,
                PROJECTOR_PAGE_LIMIT + index,
            ));
        }
        let second = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-after-gap", "after gap"),
        );
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());

        projector.run_once(&repository).unwrap();
        let owner = &projector.state.state.owners["cutex.director"];
        assert_eq!(owner.cursor.as_deref(), Some(first.cursor.as_str()));
        assert_eq!(
            owner.scan_cursor.as_deref(),
            Some(first_page_boundary.unwrap().cursor.as_str()),
            "draining pending work must not rewind the global scan boundary"
        );
        projector.run_once(&repository).unwrap();
        let owner = &projector.state.state.owners["cutex.director"];
        assert_eq!(owner.cursor.as_deref(), Some(first.cursor.as_str()));
        assert_eq!(
            owner.scan_cursor.as_deref(),
            Some(second_page_boundary.unwrap().cursor.as_str())
        );
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);

        projector.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 2);
        assert_eq!(
            projector.state.state.owners["cutex.director"]
                .cursor
                .as_deref(),
            Some(second.cursor.as_str())
        );
    }

    #[test]
    fn uncertain_pending_survives_restart_without_rewinding_scanned_gap() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let first = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-uncertain", "uncertain"),
        );
        for index in 0..PROJECTOR_PAGE_LIMIT - 1 {
            append_other_owner_gap(&repository, index);
        }
        let mut second_page_boundary = None;
        for index in 0..PROJECTOR_PAGE_LIMIT {
            second_page_boundary = Some(append_other_owner_gap(
                &repository,
                PROJECTOR_PAGE_LIMIT + index,
            ));
        }
        let second = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-after-restart-gap", "after restart gap"),
        );
        let submitter = Arc::new(FakeAppServer::default());
        submitter.outcomes.lock().unwrap().extend([
            Err(anyhow::anyhow!("uncertain transport")),
            Ok(CutexUiActivityIngestionDisposition::Accepted),
            Ok(CutexUiActivityIngestionDisposition::Accepted),
        ]);

        let mut first_projector = projector(&temp, submitter.clone());
        first_projector.run_once(&repository).unwrap();
        let owner = &first_projector.state.state.owners["cutex.director"];
        assert!(owner.cursor.is_none());
        assert_eq!(owner.pending.len(), 1);
        drop(first_projector);

        let mut restarted = projector(&temp, submitter.clone());
        restarted.run_once(&repository).unwrap();
        let owner = &restarted.state.state.owners["cutex.director"];
        assert_eq!(owner.cursor.as_deref(), Some(first.cursor.as_str()));
        assert!(owner.pending.is_empty());
        assert_eq!(
            owner.scan_cursor.as_deref(),
            Some(second_page_boundary.unwrap().cursor.as_str()),
            "accepted retry must retain the newer global scan boundary"
        );

        restarted.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 2);
        assert_eq!(
            restarted.state.state.owners["cutex.director"]
                .cursor
                .as_deref(),
            Some(second.cursor.as_str())
        );
    }

    #[test]
    fn accepted_duplicate_and_stale_checkpoint_and_restart_without_resubmit() {
        for disposition in [
            CutexUiActivityIngestionDisposition::Accepted,
            CutexUiActivityIngestionDisposition::Duplicate,
            CutexUiActivityIngestionDisposition::Stale,
        ] {
            let temp = TestDir::new();
            let repository = repository(&temp);
            let event = append(
                &repository,
                "cutex.director",
                MESSAGE_SENT,
                message_params("hello"),
            );
            let submitter = Arc::new(FakeAppServer::default());
            submitter.outcomes.lock().unwrap().push(Ok(disposition));
            projector(&temp, submitter.clone())
                .run_once(&repository)
                .unwrap();
            let state: ProjectionState =
                serde_json::from_slice(&fs::read(temp.path().join("projector.json")).unwrap())
                    .unwrap();
            assert_eq!(
                state.owners["cutex.director"].cursor.as_deref(),
                Some(event.cursor.as_str())
            );
            projector(&temp, submitter.clone())
                .run_once(&repository)
                .unwrap();
            assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn offline_or_uncertain_submission_replays_after_restart_and_rebind_succeeds() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let event = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params("hello"),
        );
        let submitter = Arc::new(FakeAppServer::default());
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("owner runtime is offline")));
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("endpoint generation changed")));
        let mut first_projector = projector(&temp, submitter.clone());
        first_projector.run_once(&repository).unwrap();
        assert!(first_projector.state.state.owners["cutex.director"]
            .cursor
            .is_none());
        assert_eq!(
            first_projector.state.state.owners["cutex.director"]
                .pending
                .len(),
            1,
            "offline work must be durable before transport"
        );
        drop(first_projector);
        let mut restarted = projector(&temp, submitter.clone());
        restarted.run_once(&repository).unwrap();
        assert!(restarted.state.state.owners["cutex.director"]
            .cursor
            .is_none());
        restarted.run_once(&repository).unwrap();
        assert_eq!(
            restarted.state.state.owners["cutex.director"]
                .cursor
                .as_deref(),
            Some(event.cursor.as_str())
        );
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
    }

    #[test]
    fn messaging_v2_fields_project_instead_of_failing_the_pass() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let mut params = message_params("ignored-content");
        params["body"] = serde_json::json!("hello v2");
        params["deliveryKind"] = serde_json::json!("note");
        params["sender"] =
            serde_json::json!({"id": "cutex.director", "name": "director", "runtime": "codex"});
        params["recipient"] =
            serde_json::json!({"id": "cutex.worker", "name": null, "runtime": null});
        params["replyOnly"] = serde_json::json!(true);
        params["senderTurnId"] = serde_json::json!("turn-1");
        append(&repository, "cutex.director", MESSAGE_SENT, params);
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());
        projector.run_once(&repository).unwrap();
        let submissions = submitter.submissions.lock().unwrap();
        let previews = submissions
            .iter()
            .map(|(_, activity)| match activity {
                CutexUiActivity::OutboundInterAgentMessage(item) => item.content_preview.clone(),
                other => panic!("unexpected activity {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(previews, vec![Some("ignored-content".to_string())]);
    }

    #[test]
    fn undecodable_payload_is_a_skip_and_body_stands_in_for_content() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let valid = append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params("hello"),
        );
        let mapper = ActivityMapper::new(FakeParticipants);
        let mut broken = valid.clone();
        broken.cutex.as_mut().unwrap().params = serde_json::json!({"messageId": 7});
        assert!(mapper.map(&broken).is_err());
        assert_eq!(mapper.map_or_skip(&broken).unwrap(), MappingDisposition::Skip);
        let mut body_only = valid.clone();
        let params = &mut body_only.cutex.as_mut().unwrap().params;
        params.as_object_mut().unwrap().remove("content");
        params["body"] = serde_json::json!("body only");
        match mapper.map_or_skip(&body_only).unwrap() {
            MappingDisposition::Project(activity) => match *activity {
                CutexUiActivity::OutboundInterAgentMessage(item) => {
                    assert_eq!(item.content_preview.as_deref(), Some("body only"))
                }
                other => panic!("unexpected activity {other:?}"),
            },
            other => panic!("unexpected disposition {other:?}"),
        }
    }

    #[test]
    fn idle_passes_neither_rewrite_state_nor_resubmit() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params("hello"),
        );
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());
        projector.run_once(&repository).unwrap();
        projector.run_once(&repository).unwrap();
        let saves = projector.state.saves.get();
        let stamp = crate::platform::file_stamp::FileStamp::of(&temp.path().join("projector.json"));
        for _ in 0..5 {
            projector.run_once(&repository).unwrap();
        }
        assert_eq!(projector.state.saves.get(), saves, "idle passes must not save");
        assert_eq!(
            crate::platform::file_stamp::FileStamp::of(&temp.path().join("projector.json")),
            stamp
        );
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
        // New work ends the idle state.
        append(
            &repository,
            "cutex.director",
            MESSAGE_SENT,
            message_params_with_id("message-2", "again"),
        );
        projector.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 2);
    }

    #[test]
    fn delivered_batch_is_saved_once_not_per_item() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        let submitter = Arc::new(FakeAppServer::default());
        let mut projector = projector(&temp, submitter.clone());
        projector.run_once(&repository).unwrap();
        for index in 0..20 {
            append(
                &repository,
                "cutex.director",
                MESSAGE_SENT,
                message_params_with_id(&format!("batch-{index}"), "hello"),
            );
        }
        let saves = projector.state.saves.get();
        projector.run_once(&repository).unwrap();
        projector.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 20);
        let written = projector.state.saves.get() - saves;
        assert!(written <= 8, "twenty deliveries took {written} state writes");
    }

    #[test]
    fn cursor_only_changes_are_saved_at_most_every_interval() {
        let temp = TestDir::new();
        let store = ProjectionStateStore::open(temp.path().join("cursor.json")).unwrap();
        store.save_cursor().unwrap();
        assert_eq!(store.saves.get(), 1, "the first cursor change is written");
        for _ in 0..10 {
            store.save_cursor().unwrap();
        }
        assert_eq!(store.saves.get(), 1, "later cursor changes wait for the interval");
        assert_eq!(store.changes.get(), 12, "deferred changes still end idleness");
        store.last_save.set(Some(
            std::time::Instant::now() - PROJECTOR_CURSOR_SAVE_INTERVAL,
        ));
        store.save_cursor().unwrap();
        assert_eq!(store.saves.get(), 2);
        store.save().unwrap();
        assert_eq!(store.saves.get(), 3, "a full save is never deferred");
    }

    #[test]
    fn owner_queue_is_bounded_without_dropping_in_transport_entries() {
        let checkpoint = CutexUiActivityCheckpoint {
            stream_id: "stream".into(),
            cursor: "c".into(),
            sequence: 1,
        };
        let entry = |sequence: u64, in_transport: bool| PendingProjection {
            stream_id: "stream".into(),
            cursor: format!("c{sequence}"),
            sequence,
            delivery_class: CutexUiActivityDeliveryClass::Live,
            delivery: in_transport.then(|| CutexUiActivityDelivery {
                schema: CutexUiActivityDeliverySchema::V1,
                class: CutexUiActivityDeliveryClass::Live,
                recovered: false,
                batch_id: format!("live:stream:{sequence}:{sequence}"),
                batch_index: 0,
                batch_size: 1,
                source_checkpoint: checkpoint.clone(),
                batch_checkpoint: checkpoint.clone(),
            }),
            recovered: false,
            activity: None,
        };
        let total = PROJECTOR_MAX_PENDING_PER_OWNER as u64 + 10;
        let mut pending: Vec<_> = (0..total).map(|s| entry(s, s < 3)).collect();
        bound_pending(&mut pending);
        assert_eq!(pending.len(), PROJECTOR_MAX_PENDING_PER_OWNER);
        let kept: Vec<u64> = pending.iter().map(|p| p.sequence).collect();
        assert_eq!(&kept[..3], &[0, 1, 2], "in-transport entries stay");
        assert_eq!(kept[3], 13, "the oldest undelivered entries go");
        assert_eq!(*kept.last().unwrap(), total - 1);
        let mut small: Vec<_> = (0..5).map(|s| entry(s, false)).collect();
        bound_pending(&mut small);
        assert_eq!(small.len(), 5);
    }

    #[test]
    fn persistently_failing_owner_is_backed_off_not_retried_every_pass() {
        let temp = TestDir::new();
        let repository = repository(&temp);
        append(
            &repository,
            "cutex.stock",
            MESSAGE_SENT,
            serde_json::json!({
                "messageId": "message-1",
                "fromCutexSessionId": "cutex.stock",
                "toCutexSessionId": "cutex.worker",
                "deliveryMode": "soon",
                "content": "hello",
                "sentAt": "2026-08-28T01:02:03Z"
            }),
        );
        let submitter = Arc::new(FakeAppServer::default());
        for _ in 0..20 {
            submitter
                .outcomes
                .lock()
                .unwrap()
                .push(Err(anyhow::anyhow!("stock runtime does not support Cutex activity ingress")));
        }
        let mut projector = projector(&temp, submitter.clone());
        for _ in 0..10 {
            projector.run_once(&repository).unwrap();
        }
        let attempts = 20 - submitter.outcomes.lock().unwrap().len();
        assert_eq!(attempts, 2, "first failure retries at once, then the owner waits");
        assert!(projector.backed_off("cutex.stock", std::time::Instant::now()));
        // Once the clock expires the owner is retried and recovers.
        submitter.outcomes.lock().unwrap().clear();
        projector.owner_backoff.get_mut("cutex.stock").unwrap().until = std::time::Instant::now();
        projector.run_once(&repository).unwrap();
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
        assert!(projector.owner_backoff.is_empty());
    }

    #[test]
    fn failing_pass_backs_off_exponentially_and_logs_each_failure_once() {
        let mut failure = PassFailure::default();
        let mut reports = Vec::new();
        let mut delays = Vec::new();
        for _ in 0..10 {
            delays.push(failure.next_delay(Err("cursor expired".into()), |detail| {
                reports.push(detail.to_string())
            }));
        }
        assert_eq!(reports, vec!["cursor expired".to_string()]);
        assert_eq!(delays[0], PROJECTOR_POLL_INTERVAL * 2);
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(*delays.last().unwrap(), PROJECTOR_FAILURE_BACKOFF_MAX);
        failure.next_delay(Err("other".into()), |detail| reports.push(detail.to_string()));
        assert_eq!(reports.len(), 2);
        assert_eq!(failure.next_delay(Ok(()), |_| {}), PROJECTOR_POLL_INTERVAL);
        failure.next_delay(Err("other".into()), |detail| reports.push(detail.to_string()));
        assert_eq!(reports.len(), 3, "a failure after recovery is reported again");
    }
}
