//! Owner-scoped typed facts emitted by Cutex integration services.

use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};

use crate::agent_management::{
    AgentActionPhase, AgentManagementOutcome, AgentManagementPhaseEvent, AgentManagementResponse,
    AgentOperationKind, AgentReplacePolicy, DirectorRotateMode,
};
use crate::app_server::commands::ParticipantPresentationMetadata;
use crate::app_server::participants::{
    ParticipantMetadataResolver, RegistryParticipantMetadataResolver,
};
use crate::platform::host::current_host_name;
use crate::role_revision::CutexSessionId;

use super::model::{CutexMessage, EventCorrelation, EventEnvelope, EventSource, PendingEvent};
use super::repository::management_v2_repository;
use super::repository::{EventRepository, ReplayQuery};

#[cfg(test)]
const INTEGRATION_DEDUPE_STATE: &str = "integration-event-dedupe-v1.json";
const INTEGRATION_DEDUPE_LOCK: &str = "integration-event-dedupe-v1.lock";

#[path = "integration_event_ledger.rs"]
mod ledger;

/// Lightweight, read-only health probe. Uninitialized state is healthy;
/// missing/corrupt migrated state and unresolved pending identities are errors.
pub fn integration_dedupe_health(root: &std::path::Path) -> anyhow::Result<()> {
    ledger::health(root)
}

/// Offline downgrade only. CLI must stop the daemon and require an explicit
/// stopped acknowledgement before passing `daemon_stopped=true`. This is not
/// a remote Management API. Export replaces the v2 marker atomically, streaming
/// all committed identities; unresolved pending identities refuse downgrade.
pub fn export_integration_dedupe_v1(
    root: &std::path::Path,
    output: &std::path::Path,
    daemon_stopped: bool,
) -> anyhow::Result<u64> {
    anyhow::ensure!(
        daemon_stopped,
        "stop the daemon before exporting integration dedupe state"
    );
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(INTEGRATION_DEDUPE_LOCK))?;
    lock.try_lock_exclusive()
        .context("integration dedupe writer is active; stop the daemon")?;
    let result = ledger::export_v1(root, output);
    let _ = FileExt::unlock(&lock);
    result
}

#[cfg(test)]
pub(super) fn prepare_synthetic_downgrade_ledger(root: &std::path::Path) -> anyhow::Result<u64> {
    drop(ledger::Ledger::open(root)?);
    export_integration_dedupe_v1(root, &root.join(INTEGRATION_DEDUPE_STATE), true)
}

#[cfg(test)]
pub(super) fn reopen_synthetic_downgrade_ledger(root: &std::path::Path) -> anyhow::Result<()> {
    let state = ledger::Ledger::open(root)?;
    anyhow::ensure!(state.identity("synthetic-permanent-identity")? == Some(ledger::Identity::Committed),
        "permanent synthetic identity was lost during downgrade");
    integration_dedupe_health(root)
}

pub const AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD: &str =
    "cutex/agentManagement/actionPhaseTransitionCommitted";
pub const AGENT_MANAGEMENT_PHASE_TRANSITION_SCHEMA: &str =
    "cutex/agent-management-phase-transition/v1";
pub const PROJECT_DIRECTOR_CHANGED_METHOD: &str = "cutex/project/directorChanged";

pub fn append_project_director_changed(
    owner: &str,
    change_id: &str,
    params: Value,
) -> anyhow::Result<()> {
    append_project_director_changed_at(management_v2_repository()?, owner, change_id, params)
}
fn append_project_director_changed_at(
    repository: &EventRepository,
    owner: &str,
    change_id: &str,
    params: Value,
) -> anyhow::Result<()> {
    CutexSessionId::new(owner)
        .map_err(|error| anyhow::anyhow!("invalid Director event owner: {error:?}"))?;
    anyhow::ensure!(
        !change_id.is_empty(),
        "Director change id must not be empty"
    );
    append_owner_event_once_with_repository_policy(
        repository,
        owner,
        PROJECT_DIRECTOR_CHANGED_METHOD,
        change_id,
        params,
        EventCorrelation {
            management_request_id: Some(change_id.into()),
            ..Default::default()
        },
        MissingCommittedEventPolicy::AcceptCommittedOutsideRetention,
    )?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagementPhaseTransitionFact {
    pub schema: String,
    pub phase_event_id: String,
    pub action_id: String,
    pub project_id: String,
    pub operation: AgentOperationKind,
    pub phase: AgentActionPhase,
    pub phase_sequence: u64,
    pub committed_at: String,
    pub primary_presentation_target_cutex_session_id: String,
    pub primary_presentation_target_metadata: Option<ParticipantPresentationMetadata>,
    #[serde(default)]
    pub subject_cutex_session_id: Option<String>,
    #[serde(default)]
    pub subject_agent_name: Option<String>,
    pub predecessor_cutex_session_id: Option<String>,
    pub predecessor_metadata: Option<ParticipantPresentationMetadata>,
    pub successor_cutex_session_id: Option<String>,
    pub successor_metadata: Option<ParticipantPresentationMetadata>,
    pub replace_policy: Option<AgentReplacePolicy>,
    pub rotation_mode: Option<DirectorRotateMode>,
    pub authority_epoch: Option<u64>,
}

pub fn append_agent_management_phase(
    phase: &AgentManagementPhaseEvent,
) -> anyhow::Result<EventEnvelope> {
    let resolver = RegistryParticipantMetadataResolver;
    let owner = phase.presentation_owner_cutex_session_id.as_str();
    let fact = AgentManagementPhaseTransitionFact {
        schema: AGENT_MANAGEMENT_PHASE_TRANSITION_SCHEMA.to_string(),
        phase_event_id: phase.event_id.clone(),
        action_id: phase.action_id.as_str().to_string(),
        project_id: phase.project_id.as_str().to_string(),
        operation: phase.operation,
        phase: phase.phase,
        phase_sequence: phase.phase_sequence,
        committed_at: phase.committed_at.as_str().to_string(),
        primary_presentation_target_cutex_session_id: owner.to_string(),
        primary_presentation_target_metadata: Some(resolver.resolve(owner)),
        subject_cutex_session_id: phase
            .subject_cutex_session_id
            .as_ref()
            .map(|session| session.as_str().to_string()),
        subject_agent_name: phase.subject_agent_name.clone(),
        predecessor_cutex_session_id: phase
            .predecessor_cutex_session_id
            .as_ref()
            .map(|session| session.as_str().to_string()),
        predecessor_metadata: phase
            .predecessor_cutex_session_id
            .as_ref()
            .map(|session| resolver.resolve(session.as_str())),
        successor_cutex_session_id: phase
            .successor_cutex_session_id
            .as_ref()
            .map(|session| session.as_str().to_string()),
        successor_metadata: phase
            .successor_cutex_session_id
            .as_ref()
            .map(|session| resolver.resolve(session.as_str())),
        replace_policy: phase.replace_policy,
        rotation_mode: phase.rotation_mode,
        authority_epoch: phase.authority_epoch,
    };
    let repository = management_v2_repository()?;
    append_owner_event_once_with_repository(
        repository,
        owner,
        AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
        &phase.event_id,
        serde_json::to_value(fact)?,
        EventCorrelation {
            // Phase identity, rather than only the parent action, makes
            // cross-process recovery idempotent for every committed phase.
            management_request_id: Some(phase.event_id.clone()),
            ..Default::default()
        },
    )
}

// Committed identities never expire. These tombstones prevent recovery from
// rematerializing owner events after envelopes leave the bounded journal.
#[cfg(test)]
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IntegrationEventDedupeState {
    #[serde(default)]
    committed: BTreeSet<String>,
    #[serde(default)]
    pending: BTreeSet<String>,
}

pub fn append_agent_management_outcome(
    caller: &CutexSessionId,
    response: &AgentManagementResponse,
) -> anyhow::Result<Option<EventEnvelope>> {
    let (method, params, action_id) = match &response.outcome {
        AgentManagementOutcome::Complete { receipt } => (
            "cutex/agentManagement/actionCompleted",
            serde_json::to_value(receipt)?,
            receipt.action_id.as_str(),
        ),
        AgentManagementOutcome::OwnerActionRequired { failure } => (
            "cutex/agentManagement/actionFailed",
            serde_json::to_value(failure)?,
            failure.action_id.as_str(),
        ),
        AgentManagementOutcome::NoWrite { .. } => return Ok(None),
    };
    append_owner_event(
        caller.as_str(),
        method,
        params,
        EventCorrelation {
            management_request_id: Some(action_id.to_string()),
            ..Default::default()
        },
    )
    .map(Some)
}

/// Records an authenticated terminal observation, not delivery/read acknowledgement.
pub(crate) fn ensure_job_terminal_observed(
    request: &crate::agent_bus::job_completion::CompletionV2,
) -> anyhow::Result<()> {
    request.validate()?;
    let _ = append_owner_event_once_with_repository_policy(
        management_v2_repository()?,
        &request.target_cutex_session_id,
        "cutex/jobService/terminalObserved",
        &request.event_id,
        serde_json::to_value(request)?,
        EventCorrelation {
            management_request_id: Some(request.event_id.clone()),
            ..Default::default()
        },
        MissingCommittedEventPolicy::AcceptCommittedOutsideRetention,
    )?;
    Ok(())
}

pub fn append_agent_bus_message_sent(
    sender: &CutexSessionId,
    message_id: &str,
    params: Value,
) -> anyhow::Result<EventEnvelope> {
    append_owner_event(
        sender.as_str(),
        "cutex/agentBus/messageSent",
        params,
        EventCorrelation {
            agent_bus_message_id: Some(message_id.to_string()),
            ..Default::default()
        },
    )
}

fn append_owner_event(
    owner_cutex_session_id: &str,
    method: &str,
    params: Value,
    correlation: EventCorrelation,
) -> anyhow::Result<EventEnvelope> {
    if owner_cutex_session_id.trim().is_empty() {
        anyhow::bail!("integration event owner must not be empty");
    }
    management_v2_repository()?
        .append(PendingEvent {
            cutex_session_id: owner_cutex_session_id.to_string(),
            host_id: current_host_name(),
            source: EventSource::Cutex,
            schema: None,
            correlation,
            native: None,
            cutex: Some(CutexMessage {
                method: method.to_string(),
                params,
            }),
        })
        .with_context(|| format!("failed to append {method} Management v2 event"))
}

fn append_owner_event_once_with_repository(
    repository: &EventRepository,
    owner_cutex_session_id: &str,
    method: &str,
    action_id: &str,
    params: Value,
    correlation: EventCorrelation,
) -> anyhow::Result<EventEnvelope> {
    append_owner_event_once_with_repository_policy(
        repository,
        owner_cutex_session_id,
        method,
        action_id,
        params,
        correlation,
        MissingCommittedEventPolicy::RequireRetainedEvent,
    )?
    .context("committed integration event is no longer retained")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MissingCommittedEventPolicy {
    RequireRetainedEvent,
    AcceptCommittedOutsideRetention,
}

fn append_owner_event_once_with_repository_policy(
    repository: &EventRepository,
    owner_cutex_session_id: &str,
    method: &str,
    action_id: &str,
    params: Value,
    correlation: EventCorrelation,
    missing_committed_event: MissingCommittedEventPolicy,
) -> anyhow::Result<Option<EventEnvelope>> {
    let key = format!("{owner_cutex_session_id}\u{0}{method}\u{0}{action_id}");
    let lock_path = repository.root().join(INTEGRATION_DEDUPE_LOCK);
    fs::create_dir_all(repository.root())?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;
    let result = (|| -> anyhow::Result<Option<EventEnvelope>> {
        let loaded = ledger::Ledger::open(repository.root())
            .inspect_err(|_| ledger::record_warning(repository.root()));
        let state = match loaded {
            Ok(state) => state,
            Err(error) => {
                // Do not overwrite unreadable/future state or invent a second
                // event when its original may already have expired. A job's
                // terminal observation is optional presentation: skip that fact
                // while allowing durable completion-message storage to continue.
                eprintln!("health warning: integration event dedupe unavailable: {error:#}");
                return match find_integration_event(repository, owner_cutex_session_id, method, action_id)? {
                Some(event) => Ok(Some(event)),
                None if missing_committed_event == MissingCommittedEventPolicy::AcceptCommittedOutsideRetention => Ok(None),
                None => anyhow::bail!("integration event dedupe state is unreadable; prior commit cannot be determined"),
            };
            }
        };
        // The obsolete timestamp sidecar is advisory only and is deliberately
        // neither read nor rewritten. Its corruption cannot block completion.
        let identity = state
            .identity(&key)
            .inspect_err(|_| ledger::record_warning(repository.root()))?;
        if identity == Some(ledger::Identity::Committed) {
            return match find_integration_event(
                repository,
                owner_cutex_session_id,
                method,
                action_id,
            )? {
                Some(event) => Ok(Some(event)),
                None if missing_committed_event
                    == MissingCommittedEventPolicy::AcceptCommittedOutsideRetention =>
                {
                    // `committed` is written only after EventRepository::append has
                    // durably committed the envelope. Management v2 intentionally
                    // expires exact cursors, so a presentation-only watchdog replay
                    // may acknowledge that durable commit without inventing a new
                    // envelope or extending the bounded event retention window.
                    Ok(None)
                }
                None => anyhow::bail!("committed integration event is no longer retained"),
            };
        }
        if let Some(existing) =
            find_integration_event(repository, owner_cutex_session_id, method, action_id)?
        {
            state
                .commit(&key)
                .inspect_err(|_| ledger::record_warning(repository.root()))?;
            return Ok(Some(existing));
        }
        if identity == Some(ledger::Identity::Pending) {
            // The process may have died after the journal fsync, and its
            // envelope may already have expired. Absence is not proof that
            // append did not commit. Keep the identity for reconciliation.
            eprintln!("health warning: integration event dedupe has unresolved pending identity; refusing to rematerialize owner fact");
            return match missing_committed_event {
                MissingCommittedEventPolicy::AcceptCommittedOutsideRetention => Ok(None),
                MissingCommittedEventPolicy::RequireRetainedEvent => anyhow::bail!(
                    "integration event pending identity is unresolved; prior commit cannot be determined"
                ),
            };
        }
        state
            .pending(&key)
            .inspect_err(|_| ledger::record_warning(repository.root()))?;
        let event = repository.append(PendingEvent {
            cutex_session_id: owner_cutex_session_id.to_string(),
            host_id: current_host_name(),
            source: EventSource::Cutex,
            schema: None,
            correlation,
            native: None,
            cutex: Some(CutexMessage {
                method: method.to_string(),
                params,
            }),
        })?;
        state
            .commit(&key)
            .inspect_err(|_| ledger::record_warning(repository.root()))?;
        Ok(Some(event))
    })();
    let _ = FileExt::unlock(&lock);
    if let Err(error) = &result {
        eprintln!("health warning: integration event dedupe unavailable; owner fact retained for retry: {error:#}");
    }
    result
}

fn find_integration_event(
    repository: &EventRepository,
    owner_cutex_session_id: &str,
    method: &str,
    action_id: &str,
) -> anyhow::Result<Option<EventEnvelope>> {
    let metadata = repository.stream_metadata()?;
    let mut after = None;
    loop {
        let page = repository
            .page(ReplayQuery {
                stream_id: Some(metadata.stream_id.clone()),
                after,
                limit: 1000,
                cutex_session_id: Some(owner_cutex_session_id.to_string()),
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if let Some(event) = page.events.into_iter().find(|event| {
            event
                .cutex
                .as_ref()
                .is_some_and(|message| message.method == method)
                && event.correlation.management_request_id.as_deref() == Some(action_id)
        }) {
            return Ok(Some(event));
        }
        if !page.has_more {
            return Ok(None);
        }
        after = page.next_cursor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_pending_never_reappends_and_blocks_unsafe_downgrade() {
        let root =
            std::env::temp_dir().join(format!("cutex-pending-unresolved-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let (owner, method, action) = ("cutex.owner", "cutex/test/uncertain", "uncertain-1");
        let state = ledger::Ledger::open(&root).unwrap();
        state
            .pending(&format!("{owner}\u{0}{method}\u{0}{action}"))
            .unwrap();
        drop(state);
        let before = repository.checkpoint().unwrap();
        let strict = append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            action,
            serde_json::json!({}),
            EventCorrelation {
                management_request_id: Some(action.into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(strict
            .to_string()
            .contains("pending identity is unresolved"));
        assert!(append_owner_event_once_with_repository_policy(
            &repository,
            owner,
            method,
            action,
            serde_json::json!({}),
            Default::default(),
            MissingCommittedEventPolicy::AcceptCommittedOutsideRetention,
        )
        .unwrap()
        .is_none());
        assert_eq!(repository.checkpoint().unwrap(), before);
        assert!(integration_dedupe_health(&root)
            .unwrap_err()
            .to_string()
            .contains("pending"));
        let output = root.join(INTEGRATION_DEDUPE_STATE);
        assert!(export_integration_dedupe_v1(&root, &output, false)
            .unwrap_err()
            .to_string()
            .contains("stop the daemon"));
        assert!(export_integration_dedupe_v1(&root, &output, true)
            .unwrap_err()
            .to_string()
            .contains("pending"));
        drop(repository);
        fs::remove_dir_all(root).unwrap();
    }
    use std::sync::{Arc, Barrier};

    #[test]
    fn director_change_retry_is_one_owner_scoped_fact_with_the_original_payload() {
        let root = std::env::temp_dir().join(format!("director-event-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let owner = "cutex.11111111-1111-4111-8111-111111111111";
        let params = serde_json::json!({"projectId":"project","newDirector":owner,"newEpoch":2});
        for _ in 0..2 {
            append_project_director_changed_at(&repository, owner, "change1", params.clone())
                .unwrap();
        }
        let metadata = repository.stream_metadata().unwrap();
        let page = repository
            .page(ReplayQuery {
                stream_id: Some(metadata.stream_id),
                after: None,
                limit: 1000,
                cutex_session_id: Some(owner.into()),
            })
            .unwrap();
        assert_eq!(page.events.len(), 1);
        let event = &page.events[0];
        assert_eq!(event.cutex_session_id, owner);
        assert_eq!(
            event.cutex.as_ref().unwrap().method,
            PROJECT_DIRECTOR_CHANGED_METHOD
        );
        assert_eq!(event.cutex.as_ref().unwrap().params, params);
        assert_eq!(
            event.correlation.management_request_id.as_deref(),
            Some("change1")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn committed_dedupe_outlives_old_timestamps_and_unrelated_activity() {
        let root = std::env::temp_dir().join(format!("cutex-dedupe-old-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let owner = "cutex.owner";
        let method = "cutex/test/oldDedupe";
        let key = format!("{owner}\u{0}{method}\u{0}expired");
        let state_path = root.join(INTEGRATION_DEDUPE_STATE);
        crate::config::atomic::write_private_pretty_json_atomic(
            &state_path,
            &IntegrationEventDedupeState {
                committed: BTreeSet::from([key.clone()]),
                pending: BTreeSet::from(["unknown-pending".into()]),
            },
            "test dedupe",
        )
        .unwrap();
        let seen_path = root.join("integration-event-dedupe-seen-v1.json");
        let seen = serde_json::to_vec(&std::collections::BTreeMap::from([(
            key.clone(),
            chrono::Utc::now() - chrono::Duration::days(31),
        )]))
        .unwrap();
        fs::write(&seen_path, &seen).unwrap();
        append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "fresh",
            serde_json::json!({}),
            EventCorrelation {
                management_request_id: Some("fresh".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let state = ledger::Ledger::open(&root).unwrap();
        assert_eq!(
            state.identity(&key).unwrap(),
            Some(ledger::Identity::Committed)
        );
        assert_eq!(
            state.identity("unknown-pending").unwrap(),
            Some(ledger::Identity::Pending)
        );
        drop(state);
        assert_eq!(fs::read(&seen_path).unwrap(), seen);
        let before = repository.checkpoint().unwrap();
        assert!(append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "expired",
            serde_json::json!({}),
            Default::default()
        )
        .unwrap_err()
        .to_string()
        .contains("no longer retained"));
        assert!(append_owner_event_once_with_repository_policy(
            &repository,
            owner,
            method,
            "expired",
            serde_json::json!({}),
            Default::default(),
            MissingCommittedEventPolicy::AcceptCommittedOutsideRetention
        )
        .unwrap()
        .is_none());
        assert_eq!(repository.checkpoint().unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_timestamp_sidecar_does_not_block_and_replay_changes_no_files() {
        let root =
            std::env::temp_dir().join(format!("cutex-dedupe-sidecar-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let seen_path = root.join("integration-event-dedupe-seen-v1.json");
        fs::write(&seen_path, b"{corrupt timestamp sidecar").unwrap();
        let owner = "cutex.owner";
        let method = "cutex/test/sidecar";
        let correlation = EventCorrelation {
            management_request_id: Some("once".into()),
            ..Default::default()
        };
        let first = append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "once",
            serde_json::json!({}),
            correlation.clone(),
        )
        .unwrap();
        let state_path = root.join(INTEGRATION_DEDUPE_STATE);
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        for path in [&state_path, &seen_path] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        let before: Vec<_> = [&state_path, &seen_path]
            .into_iter()
            .map(|path| {
                (
                    fs::read(path).unwrap(),
                    fs::metadata(path).unwrap().modified().unwrap(),
                )
            })
            .collect();
        let replay = append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "once",
            serde_json::json!({}),
            correlation,
        )
        .unwrap();
        assert_eq!(first.event_id, replay.event_id);
        let after: Vec<_> = [&state_path, &seen_path]
            .into_iter()
            .map(|path| {
                (
                    fs::read(path).unwrap(),
                    fs::metadata(path).unwrap().modified().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            before, after,
            "replay must not rewrite/fsync unchanged ledgers"
        );
        // A sidecar that cannot even be read as a file is equally advisory.
        fs::remove_file(&seen_path).unwrap();
        fs::create_dir(&seen_path).unwrap();
        append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "new-after-unreadable-sidecar",
            serde_json::json!({}),
            EventCorrelation {
                management_request_id: Some("new-after-unreadable-sidecar".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(seen_path.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_identity_ledger_skips_optional_fact_without_losing_state_or_duplicating() {
        let root =
            std::env::temp_dir().join(format!("cutex-dedupe-corrupt-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let owner = "cutex.owner";
        let method = "cutex/test/corruptLedger";
        let correlation = EventCorrelation {
            management_request_id: Some("existing".into()),
            ..Default::default()
        };
        let first = append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "existing",
            serde_json::json!({}),
            correlation.clone(),
        )
        .unwrap();
        let path = root.join(INTEGRATION_DEDUPE_STATE);
        fs::write(&path, b"{corrupt identity ledger").unwrap();
        let before = repository.checkpoint().unwrap();
        let retained = append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "existing",
            serde_json::json!({}),
            correlation,
        )
        .unwrap();
        assert_eq!(first.event_id, retained.event_id);
        assert!(append_owner_event_once_with_repository_policy(
            &repository,
            owner,
            method,
            "unknown",
            serde_json::json!({}),
            Default::default(),
            MissingCommittedEventPolicy::AcceptCommittedOutsideRetention
        )
        .unwrap()
        .is_none());
        assert!(append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            "unknown",
            serde_json::json!({}),
            Default::default()
        )
        .unwrap_err()
        .to_string()
        .contains("prior commit cannot be determined"));
        assert_eq!(repository.checkpoint().unwrap(), before);
        assert_eq!(fs::read(&path).unwrap(), b"{corrupt identity ledger");
        // Unknown future state must also survive without being rewritten as
        // empty state. Lack of a readable ledger never authorizes a resend.
        let future = br#"{"committed":[],"pending":["unknown"],"futureVersion":2}"#;
        fs::write(&path, future).unwrap();
        assert!(append_owner_event_once_with_repository_policy(
            &repository,
            owner,
            method,
            "unknown",
            serde_json::json!({}),
            Default::default(),
            MissingCommittedEventPolicy::AcceptCommittedOutsideRetention
        )
        .unwrap()
        .is_none());
        assert_eq!(fs::read(&path).unwrap(), future);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(append_owner_event_once_with_repository_policy(
            &repository,
            owner,
            method,
            "unknown",
            serde_json::json!({}),
            Default::default(),
            MissingCommittedEventPolicy::AcceptCommittedOutsideRetention
        )
        .unwrap()
        .is_none());
        assert!(path.is_dir());
        assert_eq!(repository.checkpoint().unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn job_terminal_observation_retry_commits_one_owner_event() {
        let root = std::env::temp_dir().join(format!("cutex-job-event-{}", uuid::Uuid::new_v4()));
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let owner = "cutex.11111111-1111-4111-8111-111111111111";
        let params = serde_json::json!({
            "schema":"cutex.job_service.completion.v2", "eventId":"job-terminal:job_123:3",
            "jobId":"job_123", "jobRevision":3, "terminalStatus":"exited",
            "resultSha256":"a".repeat(64), "targetCutexSessionId":owner,
            "facts":{"factsVersion":1,"actionId":"run",
                "stdout":{"observedBytes":0,"retainedBytes":0,"truncated":false},
                "stderr":{"observedBytes":0,"retainedBytes":0,"truncated":false}},
            "outputReference":"job-output:job_123"
        });
        let mut event_id = None;
        for _ in 0..2 {
            let event = append_owner_event_once_with_repository_policy(
                &repository,
                owner,
                "cutex/jobService/terminalObserved",
                "job-terminal:job_123:3",
                params.clone(),
                EventCorrelation {
                    management_request_id: Some("job-terminal:job_123:3".into()),
                    ..Default::default()
                },
                MissingCommittedEventPolicy::AcceptCommittedOutsideRetention,
            )
            .unwrap()
            .unwrap();
            if let Some(id) = &event_id {
                assert_eq!(id, &event.event_id);
            }
            event_id = Some(event.event_id);
        }
        let page = repository
            .page(ReplayQuery {
                stream_id: None,
                after: None,
                limit: 100,
                cutex_session_id: Some(owner.into()),
            })
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(crate::notify::outbound::map_event(
            &page.events[0],
            crate::notify::session::Level::Normal,
            "worker"
        )
        .is_some());
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn management_phase_fact_replay_keeps_one_raw_frontend_event() {
        let root = std::env::temp_dir().join(format!(
            "cutex-management-phase-dedupe-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let owner = "cutex.director";
        let phase_event_id = "agent-management:replace-1:phase:2";
        let params = serde_json::json!({
            "schema": AGENT_MANAGEMENT_PHASE_TRANSITION_SCHEMA,
            "phase_event_id": phase_event_id,
            "action_id": "replace-1",
            "project_id": "project-1",
            "operation": "replace",
            "phase": "predecessor_closing",
            "phase_sequence": 2,
            "committed_at": "2026-08-28T00:00:00Z",
            "primary_presentation_target_cutex_session_id": owner,
            "primary_presentation_target_metadata": null,
            "predecessor_cutex_session_id": "cutex.worker-old",
            "predecessor_metadata": null,
            "successor_cutex_session_id": null,
            "successor_metadata": null,
            "replace_policy": "close_before_create",
            "rotation_mode": null,
            "authority_epoch": null
        });
        let correlation = EventCorrelation {
            management_request_id: Some(phase_event_id.to_string()),
            ..Default::default()
        };
        let first = append_owner_event_once_with_repository(
            &repository,
            owner,
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
            phase_event_id,
            params.clone(),
            correlation.clone(),
        )
        .unwrap();
        let replay = append_owner_event_once_with_repository(
            &repository,
            owner,
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD,
            phase_event_id,
            params,
            correlation,
        )
        .unwrap();
        assert_eq!(first.event_id, replay.event_id);
        let page = repository
            .page(ReplayQuery {
                stream_id: Some(first.stream_id.clone()),
                after: None,
                limit: 100,
                cutex_session_id: Some(owner.to_string()),
            })
            .unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(
            page.events[0].cutex.as_ref().unwrap().method,
            AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD
        );
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transition_fact_dedupe_is_cross_instance_and_crash_pending_safe() {
        let root =
            std::env::temp_dir().join(format!("cutex-integration-dedupe-{}", uuid::Uuid::new_v4()));
        let owner = "cutex.director";
        // Generic owner fact (the dedupe ledger is method-agnostic).
        let method = "cutex/test/integrationDedupe";
        let action = "fact-1";
        let params = serde_json::json!({"action_id": action, "detail": "result.md"});
        let correlation = EventCorrelation {
            management_request_id: Some(action.to_string()),
            ..Default::default()
        };
        let pending_key = format!("{owner}\u{0}{method}\u{0}{action}");
        crate::config::atomic::write_private_pretty_json_atomic(
            &root.join(INTEGRATION_DEDUPE_STATE),
            &IntegrationEventDedupeState {
                committed: BTreeSet::new(),
                pending: BTreeSet::from([pending_key]),
            },
            "test integration dedupe state",
        )
        .unwrap();
        // Simulate the crash gap: journal append durable, ledger still pending.
        // A pending identity without a retained envelope is deliberately NOT
        // retried: the envelope might already have passed retention.
        let crashed = EventRepository::open(&root, current_host_name()).unwrap();
        crashed
            .append(PendingEvent {
                cutex_session_id: owner.into(),
                host_id: current_host_name(),
                source: EventSource::Cutex,
                schema: None,
                native: None,
                correlation: correlation.clone(),
                cutex: Some(CutexMessage {
                    method: method.into(),
                    params: params.clone(),
                }),
            })
            .unwrap();
        drop(crashed);
        let barrier = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            let params = params.clone();
            let correlation = correlation.clone();
            threads.push(std::thread::spawn(move || {
                let repository = EventRepository::open(&root, current_host_name()).unwrap();
                barrier.wait();
                append_owner_event_once_with_repository(
                    &repository,
                    owner,
                    method,
                    action,
                    params,
                    correlation,
                )
                .unwrap()
            }));
        }
        let first = threads.remove(0).join().unwrap();
        let second = threads.remove(0).join().unwrap();
        assert_eq!(first.event_id, second.event_id);
        let repository = EventRepository::open(&root, current_host_name()).unwrap();
        let page = repository.page(ReplayQuery::default()).unwrap();
        assert_eq!(
            page.events
                .iter()
                .filter(|event| event
                    .cutex
                    .as_ref()
                    .is_some_and(|value| value.method == method))
                .count(),
            1
        );
        let state = ledger::Ledger::open(&root).unwrap();
        assert_eq!(
            state
                .identity(&format!("{owner}\u{0}{method}\u{0}{action}"))
                .unwrap(),
            Some(ledger::Identity::Committed)
        );
        drop(state);
        integration_dedupe_health(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn committed_fact_outlives_bounded_event_retention_without_rematerializing() {
        use crate::management::v2::repository::RepositoryOptions;

        let root = std::env::temp_dir().join(format!(
            "cutex-integration-retention-dedupe-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = EventRepository::open_with_options(
            &root,
            current_host_name(),
            RepositoryOptions {
                rotate_bytes: 900,
                retained_files: 1,
            },
        )
        .unwrap();
        let owner = "cutex.director";
        // Generic owner fact (Job Service relies on this policy).
        let method = "cutex/test/integrationRetention";
        let action = "fact_retention_regression";
        let params = serde_json::json!({"action_id": action, "detail": "retention"});
        let correlation = EventCorrelation {
            management_request_id: Some(action.to_string()),
            ..Default::default()
        };
        let committed = append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            action,
            params.clone(),
            correlation.clone(),
        )
        .unwrap();

        let mut expired = false;
        for value in 0..64 {
            repository
                .append(PendingEvent {
                    cutex_session_id: "cutex.noisy-owner".to_string(),
                    host_id: current_host_name(),
                    source: EventSource::Cutex,
                    schema: None,
                    correlation: EventCorrelation::default(),
                    native: None,
                    cutex: Some(CutexMessage {
                        method: "cutex/test/retentionPressure".to_string(),
                        params: serde_json::json!({
                            "value": value,
                            "padding": "x".repeat(700),
                        }),
                    }),
                })
                .unwrap();
            if find_integration_event(&repository, owner, method, action)
                .unwrap()
                .is_none()
            {
                expired = true;
                break;
            }
        }
        assert!(
            expired,
            "fixture must move the committed event past retention"
        );
        let before = repository.checkpoint().unwrap();

        let strict = append_owner_event_once_with_repository(
            &repository,
            owner,
            method,
            action,
            params.clone(),
            correlation.clone(),
        )
        .unwrap_err();
        assert!(strict
            .to_string()
            .contains("committed integration event is no longer retained"));

        let recovered = append_owner_event_once_with_repository_policy(
            &repository,
            owner,
            method,
            action,
            params,
            correlation,
            MissingCommittedEventPolicy::AcceptCommittedOutsideRetention,
        )
        .unwrap();
        assert!(recovered.is_none());
        assert_eq!(repository.checkpoint().unwrap(), before);
        assert!(find_integration_event(&repository, owner, method, action)
            .unwrap()
            .is_none());
        assert!(committed.sequence < before.sequence);
        fs::remove_dir_all(root).unwrap();
    }
}
