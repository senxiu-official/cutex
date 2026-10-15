//! Owner-scoped typed facts emitted by Cutex integration services.

use anyhow::Context;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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

const INTEGRATION_DEDUPE_STATE: &str = "integration-event-dedupe-v1.json";
const INTEGRATION_DEDUPE_LOCK: &str = "integration-event-dedupe-v1.lock";

pub const AGENT_MANAGEMENT_PHASE_TRANSITION_METHOD: &str =
    "cutex/agentManagement/actionPhaseTransitionCommitted";
pub const AGENT_MANAGEMENT_PHASE_TRANSITION_SCHEMA: &str =
    "cutex/agent-management-phase-transition/v1";

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
        let state_path = repository.root().join(INTEGRATION_DEDUPE_STATE);
        let mut state = match fs::read(&state_path) {
            Ok(bytes) => serde_json::from_slice::<IntegrationEventDedupeState>(&bytes)
                .context("invalid integration event dedupe state")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                IntegrationEventDedupeState::default()
            }
            Err(error) => return Err(error.into()),
        };
        if state.committed.contains(&key) {
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
            state.pending.remove(&key);
            state.committed.insert(key);
            crate::config::atomic::write_private_pretty_json_atomic(
                &state_path,
                &state,
                "integration event dedupe state",
            )?;
            return Ok(Some(existing));
        }
        state.pending.insert(key.clone());
        crate::config::atomic::write_private_pretty_json_atomic(
            &state_path,
            &state,
            "integration event dedupe state",
        )?;
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
        state.pending.remove(&key);
        state.committed.insert(key);
        crate::config::atomic::write_private_pretty_json_atomic(
            &state_path,
            &state,
            "integration event dedupe state",
        )?;
        Ok(Some(event))
    })();
    let _ = FileExt::unlock(&lock);
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
    use std::sync::{Arc, Barrier};

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
                &repository, owner, "cutex/jobService/terminalObserved", "job-terminal:job_123:3",
                params.clone(), EventCorrelation {
                    management_request_id: Some("job-terminal:job_123:3".into()),
                    ..Default::default()
                }, MissingCommittedEventPolicy::AcceptCommittedOutsideRetention,
            ).unwrap().unwrap();
            if let Some(id) = &event_id { assert_eq!(id, &event.event_id); }
            event_id = Some(event.event_id);
        }
        let page = repository.page(ReplayQuery {
            stream_id: None, after: None, limit: 100,
            cutex_session_id: Some(owner.into()),
        }).unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(crate::notify::outbound::map_event(
            &page.events[0], crate::notify::session::Level::Normal, "worker"
        ).is_some());
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
        let state: IntegrationEventDedupeState =
            serde_json::from_slice(&fs::read(root.join(INTEGRATION_DEDUPE_STATE)).unwrap())
                .unwrap();
        assert!(state.pending.is_empty());
        assert_eq!(state.committed.len(), 1);
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
